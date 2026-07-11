// runtime.rhai — Container runtime abstraction for Energize.
//
// Supports Docker, Podman, OrbStack (which uses the docker CLI), and nerdctl.
// All other library modules (docker.rhai, proxy.rhai, healthcheck.rhai,
// registry.rhai, deploy.rhai) use this module to resolve the container CLI
// command, so you configure the runtime in one place.
//
// Usage in your Energize.rhai:
//
//   import "lib/runtime" as rt;
//
//   // Use Podman everywhere:
//   rt::set_runtime("podman");
//
//   // Or auto-detect:
//   rt::set_runtime("auto");
//
//   // Then proceed normally — all lib/ functions read the configured runtime.
//   import "lib/deploy" as deploy;
//   deploy::deploy(...);
//
// Supported runtimes:
//   "docker"  — Docker CE/EE (default). Also works with OrbStack,
//               Rancher Desktop, colima, or any Docker-compatible runtime.
//   "podman"  — Podman (rootful or rootless).
//   "nerdctl" — nerdctl (containerd). Experimental.
//   "auto"    — Auto-detect: tries docker, then podman, then nerdctl.
//
// PORT NOTE (Starlark -> Rhai): the legacy module kept a mutable module-global
// `_RUNTIME` dict. In Rhai every `import` yields a FRESH module instance, so a
// module-global would NOT be visible across docker.rhai / proxy.rhai / etc.
//
// The runtime choice this run resolves to is therefore backed by the EPHEMERAL,
// in-memory-only `session_set`/`session_get`/`has_session` builtins under these
// keys, so it is shared across all imports WITHIN this run — the one thing this
// mechanism was ever meant to do. `set_runtime()` additionally mirrors the choice
// into the durable `state_set` store (same keys) purely so `nrg status`/`nrg logs`/
// `nrg app exec` — separate CLI invocations that never re-run this script — can
// later recover which runtime a deploy used. `container_cmd()`/`runtime_name()`
// deliberately read ONLY the ephemeral copy, never the persisted one: reading the
// persisted copy here would mean a runtime choice from a PREVIOUS run silently
// overrides this run's true default the moment it stops calling `set_runtime()`
// (robustness review R27).
const STATE_CMD = "nrg.runtime.cmd";
const STATE_NAME = "nrg.runtime.name";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

// set_runtime(runtime) — set the container runtime for all Energize operations.
//
// Call this at the top of your Energize.rhai, BEFORE invoking other library
// functions that read the runtime. `runtime` is one of "docker", "podman",
// "nerdctl", "auto". "auto" probes the local system for available runtimes.
//
// Rhai has no default params; call set_runtime() (0-arg) for the legacy
// default of "auto".
fn set_runtime(runtime) {
    if runtime == "auto" {
        auto_detect();
    } else if ["docker", "podman", "nerdctl"].contains(runtime) {
        session_set(global::STATE_CMD, runtime);
        session_set(global::STATE_NAME, runtime);
        // Best-effort mirror to durable state too, so `nrg status`/`nrg logs`/`nrg app exec`
        // (which never re-run this script) can later recover the runtime a deploy used.
        state_set(global::STATE_CMD, runtime);
        state_set(global::STATE_NAME, runtime);
    } else {
        throw "Unknown container runtime: " + runtime + ". Use docker, podman, nerdctl, or auto.";
    }
    print("[nrg] container runtime: " + runtime_name() + " (" + container_cmd() + ")");
}

// set_runtime() — 0-arg overload preserving the legacy default of "auto".
fn set_runtime() {
    set_runtime("auto");
}

// container_cmd() — the container CLI command (e.g. "docker" or "podman").
// All library modules use this instead of hardcoding "docker". Reads the
// ephemeral, this-run-only session store (see the PORT NOTE above), defaulting
// to "docker" if `set_runtime()`/`auto_detect()` was never called THIS run.
fn container_cmd() {
    if has_session(global::STATE_CMD) { session_get(global::STATE_CMD) } else { "docker" }
}

// runtime_name() — the human-readable runtime name (may be "orbstack").
fn runtime_name() {
    if has_session(global::STATE_NAME) { session_get(global::STATE_NAME) } else { "docker" }
}

// is_podman() — true if the runtime is Podman.
fn is_podman() {
    runtime_name() == "podman"
}

// is_docker() — true if the runtime is Docker (or Docker-compatible like OrbStack).
fn is_docker() {
    runtime_name() == "docker"
}

// ---------------------------------------------------------------------------
// Auto-detection
// ---------------------------------------------------------------------------

// auto_detect() — probe the local system for available container runtimes.
//
// DRY-RUN CAVEAT: local_exec is a MUTATING-class builtin, so under `--dry-run`
// it records + returns a synthetic ok=0 with empty stdout instead of really
// probing. That makes the FIRST branch (docker) match unconditionally in a
// dry run, so auto-detect resolves to "docker" during a plan. This is the safe
// default and matches the legacy default; run set_runtime() eagerly (before a
// dry run, or in a live run) if you need a real probe.
private fn auto_detect() {
    // Try docker first (covers Docker CE, OrbStack, colima, Rancher Desktop).
    let r = local_exec("docker info --format '{{.ID}}' 2>/dev/null");
    if r.ok {
        set_detected("docker", "docker");
        // Check whether it is actually OrbStack.
        let r2 = local_exec("docker info --format '{{.OperatingSystem}}' 2>/dev/null");
        let os = r2.stdout;
        os.make_lower();
        if r2.ok && os.contains("orbstack") {
            set_detected("docker", "orbstack");
        }
        return;
    }

    // Try podman.
    let r = local_exec("podman info --format '{{.Host.OCIRuntime.Name}}' 2>/dev/null");
    if r.ok {
        set_detected("podman", "podman");
        return;
    }

    // Try nerdctl.
    let r = local_exec("nerdctl info 2>/dev/null");
    if r.ok {
        set_detected("nerdctl", "nerdctl");
        return;
    }

    throw "No container runtime found. Install Docker, Podman, or nerdctl.";
}

// set_detected(cmd, name) — record an auto-detected runtime into both the ephemeral
// session store (this run's resolution) and the durable state store (for nrg
// status/logs/app_exec, see the PORT NOTE above).
private fn set_detected(cmd, name) {
    session_set(global::STATE_CMD, cmd);
    session_set(global::STATE_NAME, name);
    state_set(global::STATE_CMD, cmd);
    state_set(global::STATE_NAME, name);
}

// ---------------------------------------------------------------------------
// Runtime-specific helpers
// ---------------------------------------------------------------------------

// runtime_run_flags() — extra flags for `run` on the current runtime.
fn runtime_run_flags() {
    "--restart unless-stopped"
}

// runtime_exec_cmd(container_name, command) — build a container exec command.
// `container_name` is quoted (issue #10); `command` is caller-authored raw — it IS a command
// line and is passed through verbatim, same contract as docker_exec (docker.rhai).
fn runtime_exec_cmd(container_name, command) {
    container_cmd() + " exec " + sh_quote(container_name) + " " + command
}
