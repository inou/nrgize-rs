// proxy.rhai — kamal-proxy management for zero-downtime deployments.
//
// kamal-proxy is a lightweight reverse proxy that supports zero-downtime
// deployments by draining connections to the old container before switching
// traffic to the new one. This module wraps its CLI.
//
// If you're not using kamal-proxy, write a similar module for traefik, caddy,
// nginx, or any other reverse proxy.
//
// Works with Docker, Podman, OrbStack, or nerdctl via lib/runtime.rhai.
//
// Usage:
//   import "lib/proxy" as proxy;
//   proxy::proxy_boot(host, #{});
//   proxy::proxy_deploy(host, service, "localhost:3000", #{});
//
// CONTAINER-OVERLAY CONTRACT: the running-check reads through
// sim_container_running and the traffic switch goes through sim_proxy_switch,
// so a dry run stays consistent with the simulated container world.

import "lib/runtime" as rt;

const KAMAL_PROXY_CONTAINER = "kamal-proxy";

// ---------------------------------------------------------------------------
// Boot / Install
// ---------------------------------------------------------------------------

// proxy_boot(host, cfg) — ensure kamal-proxy is running on a host.
//   cfg: #{http_port: 80, https_port: 443, proxy_version: "v0.9.2"}  (ports reserved; host
//        networking. proxy_version defaults to "latest" — the image tag pulled, e.g.
//        "basecamp/kamal-proxy:v0.9.2")
// Pulls the image and starts the proxy if not already running; no-op if it is.
fn proxy_boot(host, cfg) {
    let _cmd = rt::container_cmd();
    let version = if cfg.contains("proxy_version") { cfg.proxy_version } else { "latest" };
    let image = "basecamp/kamal-proxy:" + version;

    // Sim-routed running check (honest read under dry-run).
    if sim_container_running(host, global::KAMAL_PROXY_CONTAINER) {
        return; // Already running.
    }

    // Fable review (full-project pass): kamal-proxy actually HOLDS production traffic, so
    // booting it from a mutable "latest" tag with no version control is the same R10-class
    // mutable-tag risk `deploy()` already warns about for app images — a broken upstream push
    // auto-installs onto every new host, and hosts booted at different times can silently end
    // up on DIFFERENT kamal-proxy versions. Soft warning, not a refusal (matching R10's own
    // convention) — "latest" is still a reasonable default for a quick start.
    if version.to_lower() == "latest" {
        print(
            "  [warn] booting kamal-proxy from the mutable \"latest\" tag — pin cfg.proxy_version " +
            "(e.g. \"v0.9.2\") for production, so a broken upstream push can't auto-install and " +
            "every host stays on the same version."
        );
    }

    // Pull the proxy image. Robustness review R25: this used to discard the result — a
    // failed pull (network blip, registry auth, rate limit) would silently fall through to
    // `docker run -d` below, which happily starts whatever image is ALREADY cached locally (a
    // stale prior pull, or nothing at all — a fresh host with no image cached would fail loudly
    // there instead, but a host that already has an OLDER cached copy would silently keep serving
    // proxy traffic from it forever, never getting the version the caller asked for).
    let pr = ssh_exec(host, _cmd + " pull " + sh_quote(image));
    if !pr.ok {
        throw "Failed to pull " + image + " on " + host + ":\n" + pr.stderr;
    }

    // Remove any stopped proxy container (sim-routed so the sim stays consistent).
    let rm_cmd = _cmd + " rm -f " + global::KAMAL_PROXY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::KAMAL_PROXY_CONTAINER, rm_cmd);

    // Start proxy with host networking for port 80/443.
    let cmd = join([
        _cmd + " run -d",
        "--name " + global::KAMAL_PROXY_CONTAINER,
        rt::runtime_run_flags(),
        "--network host",
        "-v kamal-proxy-config:/home/kamal-proxy/.config/kamal-proxy",
        sh_quote(image),
    ], " ");
    let r = sim_docker_run(host, image, global::KAMAL_PROXY_CONTAINER, cmd);
    if !r.ok {
        throw "Failed to start kamal-proxy on " + host + ":\n" + r.stderr;
    }
}

// proxy_boot(host) — 1-arg overload (default ports).
fn proxy_boot(host) {
    proxy_boot(host, #{});
}

// proxy_boot_all(hosts, cfg) — boot kamal-proxy on all hosts.
fn proxy_boot_all(hosts, cfg) {
    for host in hosts {
        proxy_boot(host, cfg);
    }
}

// proxy_boot_all(hosts) — 1-arg overload.
fn proxy_boot_all(hosts) {
    proxy_boot_all(hosts, #{});
}

// ---------------------------------------------------------------------------
// Deploy (zero-downtime traffic switch)
// ---------------------------------------------------------------------------

// proxy_deploy(host, service, target, cfg) — switch traffic for `service` to
// `target` (host:port), draining old connections gracefully.
//   cfg: #{health_path: "/up", buffer_requests: true, buffer_timeout: 30}
// Returns the ExecResult; throws on failure.
fn proxy_deploy(host, service, target, cfg) {
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    let buffer_requests = if cfg.contains("buffer_requests") { cfg.buffer_requests } else { true };
    let buffer_timeout = if cfg.contains("buffer_timeout") { cfg.buffer_timeout } else { 30 };

    // service / target / health_path flow from user input — sh_quote them (issue #10). The
    // container name and the literal flags are trusted constants.
    let parts = [
        rt::container_cmd() + " exec " + global::KAMAL_PROXY_CONTAINER,
        "kamal-proxy deploy",
        sh_quote(service),
        "--target " + sh_quote(target),
    ];
    if health_path != "" {
        parts.push("--health-check-path " + sh_quote(health_path));
    }
    if buffer_requests {
        parts.push("--buffer-requests");
        parts.push("--buffer-timeout " + sh_quote("" + buffer_timeout + "s"));
    }

    let cmd = join(parts, " ");
    let r = sim_proxy_switch(host, service, target, cmd);
    if !r.ok {
        throw "kamal-proxy deploy failed on " + host + " for " + service + ":\n" + r.stderr;
    }
    r
}

// proxy_deploy(host, service, target) — 3-arg overload (defaults).
fn proxy_deploy(host, service, target) {
    proxy_deploy(host, service, target, #{})
}

// ---------------------------------------------------------------------------
// Remove service
// ---------------------------------------------------------------------------

// proxy_remove(host, service) — remove a service from kamal-proxy routing.
fn proxy_remove(host, service) {
    let cmd = rt::container_cmd() + " exec " + global::KAMAL_PROXY_CONTAINER + " kamal-proxy remove " + sh_quote(service);
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// TLS / Let's Encrypt
// ---------------------------------------------------------------------------

// proxy_set_tls(host, service, domain) — automatic TLS for a service via
// Let's Encrypt.
fn proxy_set_tls(host, service, domain) {
    let cmd = join([
        rt::container_cmd() + " exec " + global::KAMAL_PROXY_CONTAINER,
        "kamal-proxy deploy",
        sh_quote(service),
        "--host " + sh_quote(domain),
        "--tls",
    ], " ");
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// Maintenance mode
// ---------------------------------------------------------------------------

// proxy_maintenance(host, service, on_off, cfg) — kamal-proxy's native maintenance page.
//   on_off == true:  kamal-proxy responds 503 to every request for `service` while draining
//                     in-flight requests, WITHOUT touching the registered target — resuming
//                     restores traffic to whatever target was already there.
//   on_off == false: resume normal routing.
//   cfg: #{ drain_timeout: "30s", message: "..." }  (both only used when on_off == true;
//        message is passed through to kamal-proxy's own `--message` flag, unlike Caddy's
//        cfg.status_code, which kamal-proxy has no equivalent for)
fn proxy_maintenance(host, service, on_off, cfg) {
    let base = rt::container_cmd() + " exec " + global::KAMAL_PROXY_CONTAINER + " kamal-proxy ";
    if on_off {
        let drain_timeout = if cfg.contains("drain_timeout") { cfg.drain_timeout } else { "30s" };
        // `--drain-timeout` is a Go time.Duration flag ("30s", "1m") — it does NOT parse a bare
        // number ("30" errors "missing unit in duration"). A plausible caller mistake like
        // `#{ drain_timeout: 30 }` (an int, meaning "30 seconds") must have "s" appended, same as
        // proxy_deploy's buffer_timeout; a caller-supplied STRING is trusted as already carrying
        // its own unit (e.g. "10s", "1m") and passed through unchanged.
        let drain_timeout_str = if type_of(drain_timeout) == "string" { drain_timeout } else { "" + drain_timeout + "s" };
        let parts = [
            base + "stop", sh_quote(service), "--drain-timeout=" + sh_quote(drain_timeout_str),
        ];
        if cfg.contains("message") {
            parts.push("--message " + sh_quote(cfg.message));
        }
        let cmd = join(parts, " ");
        let r = ssh_exec(host, cmd);
        if !r.ok {
            throw "Failed to put " + service + " into maintenance mode on " + host + ":\n" + r.stderr;
        }
        r
    } else {
        let cmd = base + "resume " + sh_quote(service);
        let r = ssh_exec(host, cmd);
        if !r.ok {
            throw "Failed to resume " + service + " out of maintenance mode on " + host + ":\n" + r.stderr;
        }
        r
    }
}

// proxy_maintenance(host, service, on_off) — 3-arg overload (defaults).
fn proxy_maintenance(host, service, on_off) {
    proxy_maintenance(host, service, on_off, #{})
}

// ---------------------------------------------------------------------------
// Status / Info
// ---------------------------------------------------------------------------

// proxy_list(host) — list services registered with kamal-proxy.
// Informational only (no caller branches on the output), so plain ssh_exec.
fn proxy_list(host) {
    ssh_exec(host, rt::container_cmd() + " exec " + global::KAMAL_PROXY_CONTAINER + " kamal-proxy list")
}

// proxy_stop(host) — stop and remove the kamal-proxy container.
fn proxy_stop(host) {
    let _cmd = rt::container_cmd();
    let stop_cmd = _cmd + " stop " + global::KAMAL_PROXY_CONTAINER + " 2>/dev/null || true";
    sim_docker_stop(host, global::KAMAL_PROXY_CONTAINER, stop_cmd);
    let rm_cmd = _cmd + " rm " + global::KAMAL_PROXY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::KAMAL_PROXY_CONTAINER, rm_cmd)
}
