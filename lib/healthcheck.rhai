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
//
// This GETs `url` from the CONTROL machine — correct for checking something the control
// machine can genuinely reach directly (a public prod URL, a load balancer, anything not
// gated behind the deploy target's own SSH-only network boundary). For checking the deploy
// target HOST ITSELF, use `wait_healthy_on_host(host, port, cfg)` below instead (robustness
// review R7-health) — `deploy()`'s own per-host health gate does.
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
// HTTP health check, run ON THE HOST (robustness review R7-health)
// ---------------------------------------------------------------------------

// Robustness review R7-health: wait_healthy's GET runs from the CONTROL machine against
// `http://<ssh-host-string>:<port><path>` — but the SSH host string is often NOT a valid,
// HTTP-reachable name: a `user@host` alias (as documented for `web_hosts`) has userinfo, which
// isn't valid in an HTTP authority, and even a plain hostname is commonly firewalled to just
// 22/tcp from the control network, with the app's ephemeral host port never exposed to anything
// but the host itself. Either way, a perfectly healthy container fails the health-wait and
// unwinds an otherwise-successful fleet. `wait_healthy_on_host` closes this by running the check
// FROM the host itself, over the SAME ssh connection everything else here already uses.

// The raw probe: curl runs ON `host` (via `ssh_exec`, not a control-machine HTTP GET) against
// `http://localhost:<port><path>` — only SSH connectivity is required, matching every other
// remote operation in this stdlib. `-w '%{http_code}'` (the SAME idiom lib/caddy.rhai's own
// admin-API calls already use to capture a real status without `-f` masking it) prints ONLY the
// numeric status to stdout; curl reports "000" for a total connection failure (e.g. the
// container isn't listening yet), which parses to 0 — the SAME "no real HTTP response" convention
// `src/engine/builtins/http.rs`'s `do_get` already uses for a transport failure, so 0 never
// collides with a genuine status code.
private fn ssh_http_status(host, port, path, timeout) {
    // Dry-run: the new container isn't actually running yet (its port is symbolic), so a real
    // probe would always fail — short-circuit to the SAME "assume healthy" convention
    // `sim_http_healthy` uses for the identical not-yet-started-container scenario. No `ssh_exec`
    // call at all: there's no real command that would have run, so nothing to record.
    if is_dry_run() {
        return 200;
    }
    let cmd = "curl -s -o /dev/null -w '%{http_code}' --max-time " + timeout + " " +
              sh_quote("http://localhost:" + port + path);
    let r = ssh_exec(host, cmd);
    if !r.ok {
        return 0; // SSH-level failure to even reach the host at all.
    }
    // NOTE: `try { parse_int(...) } catch (err) { 0 }` does NOT work here — Rhai's try/catch is a
    // STATEMENT, not a value-producing expression like `if`/`else`, so using it as this function's
    // final expression silently evaluates to unit regardless of which branch runs (confirmed
    // empirically; caught by this fix's own mutation testing). Assign into an outer-scope `code`
    // from inside each branch instead, then return that.
    //
    // Also: `.trim()` mutates its string IN PLACE and returns unit (confirmed empirically) — not
    // a pure function returning the trimmed copy — so it must be its own statement; passing
    // `r.stdout.trim()` directly into `parse_int(...)` would silently pass `parse_int(())`
    // instead, throwing every time regardless of what curl actually reported.
    let code = 0;
    let out = r.stdout;
    out.trim();
    try {
        code = parse_int(out);
    } catch (err) {
        code = 0; // curl produced non-numeric output (e.g. missing on the host) — treat like a
                  // transport failure rather than let a parse error crash the whole retry loop.
    }
    code
}

// wait_healthy_on_host(host, port, cfg) — like wait_healthy, but probes `host` ITSELF (over SSH,
// against its own localhost:<port>) instead of GETting from the control machine.
//   cfg: #{path: "/up", attempts: 30, interval: 2, expected_status: 200, consecutive: 1, timeout: 30}
// Returns #{status: <int>}; throws after exhaustion. Same consecutive-success-window and
// per-request timeout semantics as `wait_healthy` (robustness review R12).
fn wait_healthy_on_host(host, port, cfg) {
    let path = if cfg.contains("path") { cfg.path } else { "/up" };
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };
    let expected_status = if cfg.contains("expected_status") { cfg.expected_status } else { 200 };
    let consecutive = if cfg.contains("consecutive") { cfg.consecutive } else { 1 };
    let timeout = if cfg.contains("timeout") { cfg.timeout } else { 30 };

    if attempts < 1 {
        throw "wait_healthy_on_host: cfg.attempts must be >= 1 (got " + attempts + ").";
    }
    if consecutive < 1 {
        throw "wait_healthy_on_host: cfg.consecutive must be >= 1 (got " + consecutive + ").";
    }
    if timeout < 1 {
        throw "wait_healthy_on_host: cfg.timeout must be >= 1 (got " + timeout + ").";
    }

    let status = 0;
    let streak = 0;
    for i in 0..attempts {
        status = ssh_http_status(host, port, path, timeout);
        if status == expected_status {
            streak += 1;
            if streak >= consecutive {
                print(
                    "  health check passed on " + host + " (" + (i + 1) + "/" + attempts + ", " +
                    streak + "/" + consecutive + " consecutive)"
                );
                return #{ status: status };
            }
        } else {
            streak = 0;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "Health check failed on " + host + " after " + attempts + " attempts: http://localhost:" +
          port + path + " (last status: " + status + ", needed " + consecutive +
          " consecutive pass(es))";
}

// wait_healthy_on_host(host, port) — 2-arg overload (defaults).
fn wait_healthy_on_host(host, port) {
    wait_healthy_on_host(host, port, #{})
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
// expected_status: 200, consecutive: 1, timeout: 30}.
//
// Robustness review R7-health: this used to build `http://<host>:<port><path>` and GET it from
// the control machine (the exact bug `wait_healthy_on_host` exists to avoid) — the SAME cfg keys
// this function already accepts map 1:1 onto `wait_healthy_on_host`'s, so it now checks each host
// from ON that host instead, over SSH.
fn wait_healthy_all(hosts, port, cfg) {
    for host in hosts {
        print("  checking " + host + "...");
        wait_healthy_on_host(host, port, cfg);
    }
}

// wait_healthy_all(hosts, port) — 2-arg overload (defaults).
fn wait_healthy_all(hosts, port) {
    wait_healthy_all(hosts, port, #{})
}
