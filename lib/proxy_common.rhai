// proxy_common.rhai — container-lifecycle helpers shared by the proxy backends
// (lib/proxy.rhai for kamal-proxy, lib/caddy.rhai for Caddy). Internal plumbing: scripts
// should import a backend, not this module.
//
// Each helper routes through the sim_* builtins, so a dry run stays consistent with the
// simulated container world. A backend's proxy_boot composes them around its own steps:
//   running check -> pull_image -> remove_stale -> (backend provisioning) -> run_container

import "lib/runtime" as rt;

// pull_image(host, image) — pull `image` on `host`; throws on failure.
// Robustness review R25: the result used to be discarded — a failed pull (network blip,
// registry auth, rate limit) would fall through to `docker run -d`, which happily starts
// whatever image is ALREADY cached locally, so a host with an OLDER cached copy would
// silently keep serving from it, never getting the version the caller asked for.
fn pull_image(host, image) {
    let pr = ssh_exec(host, rt::container_cmd() + " pull " + sh_quote(image));
    if !pr.ok {
        throw "Failed to pull " + image + " on " + host + ":\n" + pr.stderr;
    }
}

// remove_stale(host, name) — remove any stopped container called `name` (sim-routed so the
// sim stays consistent).
fn remove_stale(host, name) {
    let rm_cmd = rt::container_cmd() + " rm -f " + name + " 2>/dev/null || true";
    sim_docker_remove(host, name, rm_cmd);
}

// run_container(host, name, image, label, extra_args, command) — start `name` detached with
// host networking: `<cmd> run -d --name <name> <runtime flags> --network host <extra_args…>
// '<image>' [command]`. `extra_args` is an array of ready-to-use flag strings; `command` is
// "" for the image's default entrypoint. Throws "Failed to start <label> on <host>" on failure.
fn run_container(host, name, image, label, extra_args, command) {
    let parts = [
        rt::container_cmd() + " run -d",
        "--name " + name,
        rt::runtime_run_flags(),
        "--network host",
    ];
    for a in extra_args {
        parts.push(a);
    }
    parts.push(sh_quote(image));
    if command != "" {
        parts.push(command);
    }
    let r = sim_docker_run(host, image, name, join(parts, " "));
    if !r.ok {
        throw "Failed to start " + label + " on " + host + ":\n" + r.stderr;
    }
}

// stop_container(host, name) — stop and remove the container called `name`.
fn stop_container(host, name) {
    let _cmd = rt::container_cmd();
    let stop_cmd = _cmd + " stop " + name + " 2>/dev/null || true";
    sim_docker_stop(host, name, stop_cmd);
    let rm_cmd = _cmd + " rm " + name + " 2>/dev/null || true";
    sim_docker_remove(host, name, rm_cmd)
}
