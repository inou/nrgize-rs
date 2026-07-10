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
// status code `consecutive` times in a row.
//   cfg: #{attempts: 30, interval: 2, expected_status: 200, consecutive: 1, timeout: 30}
// Returns the last successful HttpResponse; throws after exhaustion.
fn wait_healthy(url, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };
    let expected_status = if cfg.contains("expected_status") { cfg.expected_status } else { 200 };
    // Robustness review R12: a single passing check used to be enough — an app that answers once
    // during boot then crashes (or flaps) still got traffic switched to it. `consecutive` requires
    // that many PASSING checks IN A ROW (any failure resets the streak) before returning healthy.
    // Default 1 preserves the historical single-check behavior for callers who don't set it.
    let consecutive = if cfg.contains("consecutive") { cfg.consecutive } else { 1 };
    // Robustness review R12: each probe used to have a hardcoded 30s HTTP timeout unrelated to
    // `interval` — a hanging (not erroring, just never responding) endpoint made `attempts: 30`
    // (intended as roughly a 1-minute budget at the default `interval: 2`) actually take up to
    // 30 * 30s = 15 minutes. Now forwarded to the probe so the caller's own retry budget controls
    // total wait time, not an unrelated fixed constant.
    let timeout = if cfg.contains("timeout") { cfg.timeout } else { 30 };

    // Robustness review R26: `attempts <= 0` made the loop below run zero iterations,
    // leaving `r` an empty map — then the throw read `r.status` off it. Rhai's default engine
    // config returns unit (not an error) for a missing map property, so this didn't crash — it
    // threw a confusing "Health check failed after 0 attempts: <url> (last status: )" with the
    // status silently blank, giving no hint that `attempts` itself was the actual misconfiguration.
    if attempts < 1 {
        throw "wait_healthy: cfg.attempts must be >= 1 (got " + attempts + ") (robustness review R26).";
    }
    if consecutive < 1 {
        throw "wait_healthy: cfg.consecutive must be >= 1 (got " + consecutive + ") " +
              "(robustness review R12).";
    }
    if timeout < 1 {
        throw "wait_healthy: cfg.timeout must be >= 1 (got " + timeout + ") (robustness review R12).";
    }

    // Declare r OUTSIDE the loop so the fail message can read r.status after it.
    let r = #{};
    let streak = 0;
    for i in 0..attempts {
        r = sim_http_healthy(url, timeout);
        if r.status == expected_status {
            streak += 1;
            if streak >= consecutive {
                print(
                    "  health check passed (" + (i + 1) + "/" + attempts + ", " + streak +
                    "/" + consecutive + " consecutive)"
                );
                return r;
            }
        } else {
            streak = 0;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "Health check failed after " + attempts + " attempts: " + url +
          " (last status: " + r.status + ", needed " + consecutive + " consecutive pass(es))";
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
// exhaustion in a live run. `attempts`/`interval` are the SOLE bound on total wait
// time (robustness review R11: `sim_wait_port` used to retry internally too, so this
// loop's own `attempts` silently multiplied by up to 30x — fixed, so
// `#{attempts: 5, interval: 1}` here now really is a ~5s bound, not up to 5 minutes).
fn wait_port(host, port, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };

    // Robustness review R26: same `attempts < 1` guard as wait_healthy, for consistency —
    // an empty loop here doesn't crash (no uninitialized-map read), but it silently "succeeds
    // at waiting" for zero attempts and then throws a "not open after 0 attempts" message that
    // masks the real misconfiguration just as much as wait_healthy's crash did.
    if attempts < 1 {
        throw "wait_port: cfg.attempts must be >= 1 (got " + attempts + ") (robustness review R26).";
    }

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
// Requires a HEALTHCHECK instruction in the image. Sim-routed read. `attempts`/
// `interval` are the SOLE bound on total wait time (robustness review R11 — same fix
// as wait_port above: `sim_container_healthy` used to retry internally too).
fn wait_container_healthy(host, name, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };

    // Robustness review R26: same `attempts < 1` guard as wait_healthy/wait_port, for consistency.
    if attempts < 1 {
        throw "wait_container_healthy: cfg.attempts must be >= 1 (got " + attempts + ") " +
              "(robustness review R26).";
    }

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
// expected_status: 200, consecutive: 1, timeout: 30}. URL is http://host:port/path.
fn wait_healthy_all(hosts, port, cfg) {
    let path = if cfg.contains("path") { cfg.path } else { "/up" };

    // Pass the polling knobs through to wait_healthy unchanged.
    let sub = #{};
    if cfg.contains("attempts") { sub.attempts = cfg.attempts; }
    if cfg.contains("interval") { sub.interval = cfg.interval; }
    if cfg.contains("expected_status") { sub.expected_status = cfg.expected_status; }
    if cfg.contains("consecutive") { sub.consecutive = cfg.consecutive; }
    if cfg.contains("timeout") { sub.timeout = cfg.timeout; }

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
