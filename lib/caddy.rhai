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

    ssh_exec(host, _cmd + " pull " + global::CADDY_IMAGE);

    let rm_cmd = _cmd + " rm -f " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::CADDY_CONTAINER, rm_cmd);

    // Base config: admin API bound to localhost, one server with auto-HTTPS-capable listeners
    // and an empty routes array that proxy_deploy() fills in. Delivered 0600 via the off-argv
    // stdin channel (no secrets here, but keeps the config off the command line).
    let base = "{\"admin\":{\"listen\":\"localhost:2019\"},\"apps\":{\"http\":{\"servers\":{\"srv0\":{\"listen\":[\":80\",\":443\"],\"routes\":[]}}}}}";
    write_remote(host, base, "/etc/caddy/caddy.json");

    let cmd = join([
        _cmd + " run -d",
        "--name " + global::CADDY_CONTAINER,
        rt::runtime_run_flags(),
        "--network host",
        "-v caddy-data:/data",
        "-v caddy-config:/config",
        "-v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro",
        global::CADDY_IMAGE,
        "caddy run --config /etc/caddy/caddy.json",
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

    // The route JSON (same payload for create and update so a swap preserves the shape).
    let match_part = if domain != "" {
        ",\"match\":[{\"host\":[\"" + domain + "\"]}]"
    } else {
        ""
    };
    let route = "{\"@id\":\"" + service + "\"" + match_part
        + ",\"handle\":[{\"handler\":\"reverse_proxy\",\"upstreams\":[{\"dial\":\"" + target + "\"}]}]}";

    // PATCH the existing route (replace it); if its @id doesn't exist yet (first deploy), the
    // PATCH 404s and we POST a new route to the server's route table.
    let admin = global::ADMIN;
    let cmd = "curl -fsS -X PATCH " + admin + "/id/" + service
        + " -H 'Content-Type: application/json' -d '" + route + "' 2>/dev/null"
        + " || curl -fsS -X POST " + admin + "/config/apps/http/servers/srv0/routes"
        + " -H 'Content-Type: application/json' -d '" + route + "'";

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
    let cmd = "curl -fsS -X DELETE " + global::ADMIN + "/id/" + service + " 2>/dev/null || true";
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// TLS / Let's Encrypt
// ---------------------------------------------------------------------------

// proxy_set_tls(host, service, domain) — add a host match to the service's route so Caddy's
// automatic HTTPS issues a Let's Encrypt certificate for `domain`. (The route must already
// exist — call proxy_deploy first, or just pass cfg.domain to proxy_deploy.)
fn proxy_set_tls(host, service, domain) {
    let cmd = "curl -fsS -X PATCH " + global::ADMIN + "/id/" + service + "/match"
        + " -H 'Content-Type: application/json' -d '[{\"host\":[\"" + domain + "\"]}]'";
    ssh_exec(host, cmd)
}

// ---------------------------------------------------------------------------
// Status / Info
// ---------------------------------------------------------------------------

// proxy_list(host) — show the registered routes (informational; plain ssh_exec).
fn proxy_list(host) {
    ssh_exec(host, "curl -fsS " + global::ADMIN + "/config/apps/http/servers/srv0/routes")
}

// proxy_stop(host) — stop and remove the Caddy container.
fn proxy_stop(host) {
    let _cmd = rt::container_cmd();
    let stop_cmd = _cmd + " stop " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_stop(host, global::CADDY_CONTAINER, stop_cmd);
    let rm_cmd = _cmd + " rm " + global::CADDY_CONTAINER + " 2>/dev/null || true";
    sim_docker_remove(host, global::CADDY_CONTAINER, rm_cmd)
}
