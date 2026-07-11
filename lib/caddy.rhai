// caddy.rhai — Caddy reverse-proxy management, a drop-in alternative to lib/proxy.rhai
// (kamal-proxy). Exposes the SAME surface (proxy_boot / proxy_deploy / proxy_remove /
// proxy_set_tls / proxy_list / proxy_stop / proxy_boot_all), so the fleet-atomic deploy()
// can use it instead of kamal-proxy.
//
// MODEL: Caddy runs as a container with host networking and its admin API on
// localhost:2019. Each service is one HTTP route tagged with `@id: <service>`, whose
// reverse_proxy points at a single upstream (the live container's host:port). A
// zero-downtime traffic switch is an atomic admin-API call that replaces that route's
// upstream — Caddy applies config changes gracefully (in-flight requests drain).
//
//   import "lib/caddy" as proxy;          // same name your deploy expects
//   proxy::proxy_boot(host, #{});
//   proxy::proxy_deploy(host, "app", "localhost:13000", #{ domain: "app.example.com" });
//
// Or select it from the built-in deploy():  deploy::deploy(hosts, image, "app", #{ proxy: "caddy" });
//
// TLS: pass `cfg.domain` to proxy_deploy (or call proxy_set_tls) — Caddy's automatic HTTPS
// issues a Let's Encrypt certificate for any route that has a host match.
//
// CONTAINER-OVERLAY CONTRACT: the running-check goes through sim_container_running and the
// traffic switch through sim_proxy_switch, so a `--dry-run` stays consistent with the
// simulated container world (same as lib/proxy.rhai).
//
// NOTE: this is a solid starting point. Caddy's config model is flexible; tune the base
// config / route shape below for your setup (extra middleware, headers, multiple upstreams,
// active health checks, etc.).

import "lib/runtime" as rt;

const CADDY_IMAGE = "caddy:2";
const CADDY_CONTAINER = "caddy";
const ADMIN = "http://localhost:2019";

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

// proxy_boot(host, cfg) — ensure Caddy is running on a host with the admin API enabled.
//   cfg: #{}  (reserved for future options)
// Starts Caddy (host networking) with a base config: admin API on localhost:2019 and one
// HTTP server `srv0` on :80/:443 with an empty route table. No-op if already running.
fn proxy_boot(host, cfg) {
    let _cmd = rt::container_cmd();

    // Sim-routed running check (honest read under dry-run).
    if sim_container_running(host, global::CADDY_CONTAINER) {
        return;
    }

    // Robustness review R25: this used to discard the pull's result — a failed pull (network
    // blip, registry auth, rate limit) would silently fall through to `docker run -d` below,
    // which happily starts whatever image is ALREADY cached locally instead of the fresh one the
    // caller asked for (a host with nothing cached at all would fail loudly there instead, but a
    // host with an OLDER cached copy would silently keep running it, unnoticed, indefinitely).
    let pr = ssh_exec(host, _cmd + " pull " + global::CADDY_IMAGE);
    if !pr.ok {
        throw "Failed to pull " + global::CADDY_IMAGE + " on " + host + ":\n" + pr.stderr;
    }

    let rm_cmd = _cmd + " rm -f " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::CADDY_CONTAINER, rm_cmd);

    // Base config: admin API bound to localhost, one server with auto-HTTPS-capable listeners
    // and an empty routes array that proxy_deploy() fills in. Delivered 0600 via the off-argv
    // stdin channel (no secrets here, but keeps the config off the command line).
    let base = "{\"admin\":{\"listen\":\"localhost:2019\"},\"apps\":{\"http\":{\"servers\":{\"srv0\":{\"listen\":[\":80\",\":443\"],\"routes\":[]}}}}}";
    // Checked (robustness review R3b): a non-root deploy user can't write /etc/caddy, and the
    // subsequent `docker run -d` below returns 0 regardless (Docker's `-v` just creates an empty
    // directory at that path if nothing exists, so the bind mount itself doesn't fail either) —
    // Caddy then crash-loops with no config to read, and the failure only surfaced later as an
    // opaque curl error during the traffic switch, already inside the fleet transaction.
    let wr = write_remote(host, base, "/etc/caddy/caddy.json");
    if !wr.ok {
        throw "Failed to write Caddy config on " + host + " (/etc/caddy/caddy.json):\n" + wr.stderr;
    }

    // `--resume` makes Caddy load its LAST autosaved config on restart (issue #28). Admin-API
    // route changes persist only to Caddy's autosave, which a bare `--config` does NOT reload —
    // so after a host reboot the route table would come back EMPTY and every service 404s until
    // the next deploy. With `--resume`, first boot loads the base --config (admin enabled) and
    // every subsequent boot restores the autosaved routes, matching kamal-proxy's durability.
    let cmd = join([
        _cmd + " run -d",
        "--name " + global::CADDY_CONTAINER,
        rt::runtime_run_flags(),
        "--network host",
        "-v caddy-data:/data",
        "-v caddy-config:/config",
        "-v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro",
        global::CADDY_IMAGE,
        "caddy run --resume --config /etc/caddy/caddy.json",
    ], " ");
    let r = sim_docker_run(host, global::CADDY_IMAGE, global::CADDY_CONTAINER, cmd);
    if !r.ok {
        throw "Failed to start Caddy on " + host + ":\n" + r.stderr;
    }
}

// proxy_boot(host) — 1-arg overload.
fn proxy_boot(host) {
    proxy_boot(host, #{});
}

// proxy_boot_all(hosts, cfg) — boot Caddy on all hosts.
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

// proxy_deploy(host, service, target, cfg) — point `service` at `target` (host:port) via the
// Caddy admin API. Create-or-update: PATCH the existing `@id:<service>` route's upstream, or
// POST a new route if it doesn't exist yet. Caddy applies the change gracefully (zero-downtime).
//   cfg: #{ domain: "" }   (a non-empty domain adds a host match -> automatic HTTPS)
// Returns the ExecResult; throws on failure.
fn proxy_deploy(host, service, target, cfg) {
    let domain = if cfg.contains("domain") { cfg.domain } else { "" };
    // health_path is threaded in via the shared proxy_cfg (same map as kamal). Honoring it keeps
    // the "health-gated, drain-on-switch" contract consistent across backends (issue #7/#23): the
    // deploy already waits for the new container's health BEFORE switching, and here we also add a
    // Caddy ACTIVE health check on the upstream so Caddy keeps probing it on the configured path
    // (not a hardcoded default) and pulls it from rotation if it goes unhealthy after the switch.
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "" };

    // Build the route JSON with json_string() so service/domain/target are JSON-escaped — a value
    // containing `"` can no longer break out of the JSON (issue #10). The same payload is used for
    // create and update so a swap preserves the shape.
    let match_part = if domain != "" {
        ",\"match\":[{\"host\":[" + json_string(domain) + "]}]"
    } else {
        ""
    };
    let health_part = if health_path != "" {
        ",\"health_checks\":{\"active\":{\"uri\":" + json_string(health_path)
            + ",\"interval\":\"10s\",\"timeout\":\"5s\"}}"
    } else {
        ""
    };
    let route = "{\"@id\":" + json_string(service) + match_part
        + ",\"handle\":[{\"handler\":\"reverse_proxy\",\"upstreams\":[{\"dial\":" + json_string(target) + "}]"
        + health_part + "}]}";

    // The JSON is already injection-safe (json_string escaped every value); sh_quote() then wraps
    // the WHOLE body so it can't break out of the shell either (issue #10) — belt and suspenders.
    // The route carries no secrets, so it stays inline (`-d`) rather than off-argv, which keeps it
    // visible in the dry-run plan. URL components are sh_quote()'d too.
    let admin = global::ADMIN;
    let qroute = sh_quote(route);
    // Robustness review R17: `service` addresses a URL PATH SEGMENT here, not just a shell
    // argument — sh_quote() alone stops it breaking out of the shell command, but does nothing
    // to stop a `/` or `../` inside it from addressing a DIFFERENT Caddy admin-API path than
    // intended (e.g. "x/../../config/apps" could reach an unrelated config node). url_encode()
    // makes the segment literal before it's ever assembled into a URL.
    let patch_url = sh_quote(admin + "/id/" + url_encode(service));
    let post_url = sh_quote(admin + "/config/apps/http/servers/srv0/routes");

    // PATCH the existing @id route; if it doesn't exist yet (first deploy) the PATCH 404s and we
    // POST a new route instead. Robustness review R13: `curl -fsS` turns ANY non-2xx response (or
    // a connection-level failure) into a nonzero exit, so a plain `PATCH || POST` fallback can't
    // tell "route doesn't exist yet" (404, expected) from a TRANSIENT failure on an EXISTING route
    // (a timeout, a 400, a 500) — both took the same `|| POST` branch, which APPENDS a brand-new
    // route rather than replacing the existing one. Caddy matches the FIRST route in the array, so
    // the stale upstream silently kept serving traffic while `nrg` reported success; two
    // domain-less services would each become a catch-all route and one would swallow the other.
    // Fixed: capture PATCH's HTTP status via `-w` instead of putting `-f` on it (curl still exits 0
    // on an HTTP-level error as long as it got a response at all, so `%{http_code}` is trustworthy),
    // and only fall through to POST on an EXACT "404". Any other non-2xx status — including "000",
    // which `-w` reports for a connection-level curl failure — fails loudly instead of silently
    // duplicating the route.
    let cmd = "code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH " + patch_url
        + " -H 'Content-Type: application/json' -d " + qroute + "); "
        + "case \"$code\" in "
        + "404) curl -fsS -X POST " + post_url + " -H 'Content-Type: application/json' -d " + qroute + " ;; "
        + "2??) : ;; "
        + "*) echo \"Caddy PATCH failed with HTTP $code\" >&2; exit 1 ;; "
        + "esac";

    let r = sim_proxy_switch(host, service, target, cmd);
    if !r.ok {
        throw "Caddy deploy failed on " + host + " for " + service + ":\n" + r.stderr;
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

// proxy_remove(host, service) — delete the service's route from Caddy.
fn proxy_remove(host, service) {
    // R13 follow-up (Opus review): the old `2>/dev/null || true` swallowed EVERY DELETE failure
    // the same way the pre-fix `proxy_deploy`'s `PATCH || POST` did — a transient admin-API error
    // (a timeout, a 500) looked identical to "already removed" (404, genuinely fine — removal is
    // idempotent), so a caller checking `.ok` could never tell a route that's actually still
    // there from one that's really gone. Same fix as `proxy_deploy`: check the exact HTTP status.
    // Robustness review R17: same URL-path-segment concern as proxy_deploy above — url_encode()
    // before assembling the path, not just sh_quote() for the shell.
    let cmd = "code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "
        + sh_quote(global::ADMIN + "/id/" + url_encode(service)) + "); "
        + "case \"$code\" in 404) : ;; 2??) : ;; *) echo \"Caddy DELETE failed with HTTP $code\" >&2; exit 1 ;; esac";
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// TLS / Let's Encrypt
// ---------------------------------------------------------------------------

// proxy_set_tls(host, service, domain) — add a host match to the service's route so Caddy's
// automatic HTTPS issues a Let's Encrypt certificate for `domain`. (The route must already
// exist — call proxy_deploy first, or just pass cfg.domain to proxy_deploy.)
fn proxy_set_tls(host, service, domain) {
    // JSON built with json_string (escaped), then the whole body sh_quote()'d for the shell.
    let body = "[{\"host\":[" + json_string(domain) + "]}]";
    // Robustness review R17: same URL-path-segment concern as proxy_deploy above.
    let cmd = "curl -fsS -X PATCH " + sh_quote(global::ADMIN + "/id/" + url_encode(service) + "/match")
        + " -H 'Content-Type: application/json' -d " + sh_quote(body);
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// Status / Info
// ---------------------------------------------------------------------------

// proxy_list(host) — show the registered routes (informational; plain ssh_exec).
fn proxy_list(host) {
    ssh_exec(host, "curl -fsS " + sh_quote(global::ADMIN + "/config/apps/http/servers/srv0/routes"))
}

// proxy_stop(host) — stop and remove the Caddy container.
fn proxy_stop(host) {
    let _cmd = rt::container_cmd();
    let stop_cmd = _cmd + " stop " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_stop(host, global::CADDY_CONTAINER, stop_cmd);
    let rm_cmd = _cmd + " rm " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::CADDY_CONTAINER, rm_cmd)
}
