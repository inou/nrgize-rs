// healthcheck.rhai — Health check polling for Energize deployments.
//
// Configurable retry loops for verifying a service is up after deployment.
// Supports HTTP health endpoints and TCP port / container-health checks.
//
// Usage:
//   import "lib/healthcheck" as health;
//   health::wait_healthy("http://10.0.0.1:3000/up", #{});
//   health::wait_port(host, port, #{});
//
// CONTAINER-OVERLAY CONTRACT: the TCP port probe and the container-health probe
// route through sim_wait_port / sim_container_healthy so they agree with the
// just-stubbed container under dry-run instead of probing the live host.
// wait_healthy uses sim_http_healthy (NOT http_get) because it probes the
// NOT-yet-started new container: in dry-run that returns a synthetic 200 (the
// container's symbolic port isn't live), while a live run does a real GET. Use
// http_get directly when you mean to probe EXISTING reality (e.g. a precondition
// on current prod) — http_get is honest under dry-run and really probes.

import "lib/runtime" as rt;

// ---------------------------------------------------------------------------
// HTTP health check
// ---------------------------------------------------------------------------

// wait_healthy(url, cfg) — poll an HTTP endpoint until it returns the expected
// status code.  cfg: #{attempts: 30, interval: 2, expected_status: 200}
// Returns the successful HttpResponse; throws after exhaustion.
fn wait_healthy(url, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };
    let expected_status = if cfg.contains("expected_status") { cfg.expected_status } else { 200 };

    // Declare r OUTSIDE the loop so the fail message can read r.status after it.
    let r = #{};
    for i in 0..attempts {
        r = sim_http_healthy(url);
        if r.status == expected_status {
            print("  health check passed (" + (i + 1) + "/" + attempts + ")");
            return r;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "Health check failed after " + attempts + " attempts: " + url +
          " (last status: " + r.status + ")";
}

// wait_healthy(url) — 1-arg overload (defaults).
fn wait_healthy(url) {
    wait_healthy(url, #{})
}

// ---------------------------------------------------------------------------
// TCP port check
// ---------------------------------------------------------------------------

// wait_port(host, port, cfg) -> bool — wait for a TCP port to be open on a host.
//   cfg: #{attempts: 30, interval: 2}
// Sim-routed: in dry-run agrees with the stubbed container; throws after
// exhaustion in a live run.
fn wait_port(host, port, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };

    for i in 0..attempts {
        if sim_wait_port(host, port) {
            print("  port " + port + " open on " + host + " (" + (i + 1) + "/" + attempts + ")");
            return true;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "Port " + port + " not open on " + host + " after " + attempts + " attempts";
}

// wait_port(host, port) — 2-arg overload (defaults).
fn wait_port(host, port) {
    wait_port(host, port, #{})
}

// ---------------------------------------------------------------------------
// Container health check
// ---------------------------------------------------------------------------

// wait_container_healthy(host, name, cfg) -> bool — wait for a container's
// HEALTHCHECK to report "healthy".  cfg: #{attempts: 30, interval: 2}
// Requires a HEALTHCHECK instruction in the image. Sim-routed read.
fn wait_container_healthy(host, name, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };

    for i in 0..attempts {
        if sim_container_healthy(host, name) {
            print("  container " + name + " healthy on " + host + " (" + (i + 1) + "/" + attempts + ")");
            return true;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "Container " + name + " not healthy on " + host + " after " + attempts + " attempts";
}

// wait_container_healthy(host, name) — 2-arg overload (defaults).
fn wait_container_healthy(host, name) {
    wait_container_healthy(host, name, #{})
}

// ---------------------------------------------------------------------------
// Multi-host health check
// ---------------------------------------------------------------------------

// wait_healthy_all(hosts, port, cfg) — wait for a service to be healthy on all
// hosts (sequentially).  cfg: #{path: "/up", attempts: 30, interval: 2,
// expected_status: 200}. URL is http://host:port/path.
fn wait_healthy_all(hosts, port, cfg) {
    let path = if cfg.contains("path") { cfg.path } else { "/up" };

    // Pass the polling knobs through to wait_healthy unchanged.
    let sub = #{};
    if cfg.contains("attempts") { sub.attempts = cfg.attempts; }
    if cfg.contains("interval") { sub.interval = cfg.interval; }
    if cfg.contains("expected_status") { sub.expected_status = cfg.expected_status; }

    for host in hosts {
        let url = "http://" + host + ":" + port + path;
        print("  checking " + host + "...");
        wait_healthy(url, sub);
    }
}

// wait_healthy_all(hosts, port) — 2-arg overload (defaults).
fn wait_healthy_all(hosts, port) {
    wait_healthy_all(hosts, port, #{})
}
