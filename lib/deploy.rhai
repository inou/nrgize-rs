// deploy.rhai — Fleet-atomic, zero-downtime deployment orchestration.
//
// TRUE FLEET-ATOMIC design (Phase 5c): the WHOLE rolling fleet runs inside a
// SINGLE transaction(). If ANY host fails mid-roll, the transaction unwinds the
// compensations registered so far — restoring every already-switched host's
// proxy to its OLD target and removing the new containers — so the fleet is
// never left half-deployed.
//
// Lifecycle:
//
//   OUTSIDE the transaction (no rollback needed for these):
//     1. build image locally        (skippable)
//     2. push to registry           (skippable)
//     3. pull on ALL hosts
//     4. ensure proxy is up on ALL hosts
//     5. snapshot <service>.prev = current <service>.image  (rollback wiring)
//
//   INSIDE one transaction wrapping the whole fleet — per host, sequentially:
//     a. capture the OLD proxy target BY VALUE
//     b. run the NEW container under a UNIQUE name (svc-web-<ver>-<rand>)
//     c. wait for it to be healthy  (sim-routed: agrees with the stub in dry-run)
//     d. register on_rollback(restore proxy -> OLD target)
//        register on_rollback(rm -f NEW container)   <-- BEFORE the switch
//     e. switch proxy traffic to the NEW container
//     f. the OLD container is LEFT RUNNING (under its canonical name) so a
//        rollback can flip the proxy straight back to it.
//
//   POST-COMMIT (after the transaction returns Ok, OUTSIDE it):
//     - ONE cleanup pass across the fleet: rename new->canonical, remove the old
//       container, prune.
//     - persist <service>.version / <service>.image / per-host proxy target.
//
// Per-host SSH in the rolling loop is SEQUENTIAL (plain ssh_exec / per-host
// calls), NOT ssh_exec_all — fan-out swallows per-host failures, which would
// defeat the atomic unwind.
//
// Usage:
//   import "lib/deploy" as deploy;
//   deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v42", "app", #{});
//
// IMPORTANT: imports are PER-FILE in Rhai (NOT inherited from the caller), so
// this module imports every stdlib module it touches directly. The runtime
// choice is shared across these fresh module instances via the StateStore
// (see lib/runtime.rhai), not module globals.

import "lib/docker" as docker;
import "lib/proxy" as kproxy;     // kamal-proxy (the default)
import "lib/caddy" as cproxy;     // Caddy (selected with cfg.proxy == "caddy")
import "lib/healthcheck" as health;
import "lib/runtime" as rt;

// state_set / state_get / has_state / transaction / on_rollback are GLOBAL
// builtins (no import).

// Proxy dispatch — the fleet deploy is proxy-agnostic. `cfg.proxy` selects the backend:
// "kamal" (default, lib/proxy.rhai) or "caddy" (lib/caddy.rhai). Both expose the same
// proxy_boot / proxy_deploy surface, so a Caddy module (or your own) drops right in.
fn px_boot(kind, host) {
    if kind == "caddy" { cproxy::proxy_boot(host); } else { kproxy::proxy_boot(host); }
}
fn px_deploy(kind, host, service, target, cfg) {
    if kind == "caddy" {
        cproxy::proxy_deploy(host, service, target, cfg)
    } else {
        kproxy::proxy_deploy(host, service, target, cfg)
    }
}

// ---------------------------------------------------------------------------
// Full fleet-atomic deploy
// ---------------------------------------------------------------------------

// deploy(hosts, image, service, cfg) — fleet-atomic zero-downtime deploy.
//
//   cfg: #{
//     container_port: 3000,        // port the app listens on inside the container
//     envs: #{}, volumes: #{},     // container env / volume mounts
//     health_path: "/up",          // HTTP health path (relative to container_port)
//     health_attempts: 30, health_interval: 2,
//     build_context: ".", dockerfile: "Dockerfile", build_args: #{},
//     skip_build: false, skip_push: false,
//     network: "",
//     pre_deploy_cmd: "", post_deploy_cmd: "",
//   }
//
// Throws if any host fails; the whole fleet unwinds atomically before re-raising.
fn deploy(hosts, image, service, cfg) {
    let version = extract_version(image);

    let container_port = if cfg.contains("container_port") { cfg.container_port } else { 3000 };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    let health_attempts = if cfg.contains("health_attempts") { cfg.health_attempts } else { 30 };
    let health_interval = if cfg.contains("health_interval") { cfg.health_interval } else { 2 };
    let build_context = if cfg.contains("build_context") { cfg.build_context } else { "." };
    let dockerfile = if cfg.contains("dockerfile") { cfg.dockerfile } else { "Dockerfile" };
    let build_args = if cfg.contains("build_args") { cfg.build_args } else { #{} };
    let skip_build = if cfg.contains("skip_build") { cfg.skip_build } else { false };
    let skip_push = if cfg.contains("skip_push") { cfg.skip_push } else { false };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    // pre_deploy: an IN-CONTAINER release command (e.g. "bin/rails db:migrate") run ONCE from a
    // throwaway container built on the NEW image, BEFORE the roll (issue #8). pre_deploy_cmd is
    // the legacy per-host RAW host command (kept for non-migration hooks); prefer pre_deploy for
    // anything that must run against the new image's code.
    let pre_deploy = if cfg.contains("pre_deploy") { cfg.pre_deploy } else { "" };
    let pre_deploy_cmd = if cfg.contains("pre_deploy_cmd") { cfg.pre_deploy_cmd } else { "" };
    let post_deploy_cmd = if cfg.contains("post_deploy_cmd") { cfg.post_deploy_cmd } else { "" };
    let proxy_kind = if cfg.contains("proxy") { cfg.proxy } else { "kamal" };
    let domain = if cfg.contains("domain") { cfg.domain } else { "" };

    print("==> Deploying " + service + " " + version + " to " + hosts.len() + " host(s)");

    // -------------------------------------------------------------------
    // Phase 1-2: Build & push (OUTSIDE the transaction — no rollback).
    // -------------------------------------------------------------------
    if !skip_build {
        print("\n--- Build ---");
        docker::docker_build(image, #{
            context: build_context,
            dockerfile: dockerfile,
            build_args: build_args,
        });
    }
    if !skip_push {
        print("\n--- Push ---");
        docker::docker_push(image);
    }

    // -------------------------------------------------------------------
    // Phase 3: Pull on ALL hosts (OUTSIDE the transaction). Fan-out is OK
    // here: a pull failure aborts before we touch any running container.
    // -------------------------------------------------------------------
    print("\n--- Pull ---");
    docker::docker_pull_all(hosts, image);

    // -------------------------------------------------------------------
    // Phase 3b: Pre-deploy / release command (e.g. DB migrations) — run ONCE,
    // from a THROWAWAY container built on the NEW image (issue #8). It runs
    // against the NEW code (not an exec into the still-running OLD container),
    // exactly once for the fleet, and a FAILURE THROWS (no `|| true` swallow)
    // so a bad migration aborts the deploy before any traffic switches.
    // -------------------------------------------------------------------
    if pre_deploy != "" {
        let mhost = hosts[0];
        print("\n--- Pre-deploy (new-image release task) on " + mhost + " ---");
        print("    " + pre_deploy);
        let rr = docker::docker_run_once(mhost, image, pre_deploy, #{
            envs: envs,
            network: network,
        });
        if !rr.ok {
            throw "Pre-deploy release command failed on " + mhost + ":\n" + rr.stderr;
        }
    }

    // -------------------------------------------------------------------
    // Phase 4: Ensure the proxy is up on every host (OUTSIDE the transaction).
    // -------------------------------------------------------------------
    print("\n--- Proxy ---");
    for host in hosts {
        px_boot(proxy_kind, host);
    }

    // -------------------------------------------------------------------
    // Phase 5: Snapshot the previous image UNDER the state store BEFORE the
    // rolling deploy, so rollback() has a target. Done here (not inside the
    // transaction) so the snapshot survives a successful commit.
    // -------------------------------------------------------------------
    let prev_image = if has_state(service + ".image") { state_get(service + ".image") } else { "" };
    if prev_image != "" && prev_image != image {
        state_set(service + ".prev", prev_image);
    }

    // Collect the per-host new-container names so the POST-COMMIT pass can finish
    // the swap (rename new->canonical, remove old) once the whole fleet is up.
    let rolled = [];

    // -------------------------------------------------------------------
    // Phase 6: ONE transaction wrapping the WHOLE rolling fleet.
    // -------------------------------------------------------------------
    print("\n--- Deploy (fleet-atomic) ---");
    transaction(|| {
        for host in hosts {
            print("\n  [" + host + "]");
            let result = deploy_one_host(host, image, service, #{
                version: version,
                container_port: container_port,
                envs: envs,
                volumes: volumes,
                health_path: health_path,
                health_attempts: health_attempts,
                health_interval: health_interval,
                network: network,
                pre_deploy_cmd: pre_deploy_cmd,
                proxy: proxy_kind,
                domain: domain,
            });
            rolled.push(result);
        }
    });

    // -------------------------------------------------------------------
    // POST-COMMIT (transaction returned Ok): the whole fleet switched to the
    // new containers. Now — and only now — finish the swap and remove the OLD
    // containers in a single pass across the fleet. Compensations are idempotent
    // (`rm -f ... || true`) so a partial cleanup never wedges a re-run.
    // -------------------------------------------------------------------
    print("\n--- Post-commit cleanup ---");
    let canonical = service + "-web";
    let old_name = service + "-web-old";
    for entry in rolled {
        let host = entry.host;
        let new_name = entry.new_name;
        let new_port = entry.new_port;
        // The old canonical container is still running; retire it, promote the new.
        docker::docker_rename(host, canonical, old_name);
        docker::docker_rename(host, new_name, canonical);
        docker::docker_stop(host, old_name);
        docker::docker_remove(host, old_name);
        docker::docker_cleanup(host);
        // Persist the new port + proxy target NOW (post-commit) — never inside the
        // transaction, so a mid-fleet failure can't leave a stale port behind.
        state_set(service + ".port." + host, "" + new_port);
        state_set(service + ".target." + host, "localhost:" + new_port);
    }

    // Post-deploy hook (best-effort, after the fleet is committed).
    if post_deploy_cmd != "" {
        for host in hosts {
            print("    post-deploy on " + host + ": " + post_deploy_cmd);
            ssh_exec(host, post_deploy_cmd);
        }
    }

    // -------------------------------------------------------------------
    // Phase 7: Persist deploy state, INCLUDING the full effective config, so
    // rollback() can replay it verbatim instead of reverting envs/port/health/
    // proxy to defaults (issue #6). build/push are forced off on replay. The
    // config is JSON in state; on a LIVE run this is a 0600 file (see state.rs),
    // and the dry-run plan redacts any registered secret in the blob.
    // -------------------------------------------------------------------
    let effective_cfg = #{
        container_port: container_port,
        envs: envs,
        volumes: volumes,
        health_path: health_path,
        health_attempts: health_attempts,
        health_interval: health_interval,
        network: network,
        proxy: proxy_kind,
        domain: domain,
        pre_deploy: pre_deploy,
        pre_deploy_cmd: pre_deploy_cmd,
        post_deploy_cmd: post_deploy_cmd,
    };
    state_set(service + ".config", to_json(effective_cfg));
    state_set(service + ".version", version);
    state_set(service + ".image", image);
    state_set(service + ".deployed_at", timestamp());

    print("\n==> Deploy complete: " + service + " " + version);
}

// deploy(hosts, image, service) — 3-arg overload (defaults).
fn deploy(hosts, image, service) {
    deploy(hosts, image, service, #{})
}

// ---------------------------------------------------------------------------
// Single-host roll (called from inside the fleet transaction)
// ---------------------------------------------------------------------------

// deploy_one_host(host, image, service, cfg) -> String
//
// Rolls ONE host forward: starts the NEW container, waits for health, registers
// the rollback compensations, then switches the proxy. The OLD container is
// LEFT RUNNING under its canonical name — the post-commit pass retires it once
// the whole fleet is up.
//
//   cfg: #{ version, container_port, envs, volumes, health_path,
//           health_attempts, health_interval, network, pre_deploy_cmd }
//
// Returns the unique NEW container name (so the caller can finish the swap).
// Throws on any per-host failure (which unwinds the enclosing transaction).
fn deploy_one_host(host, image, service, cfg) {
    let version = if cfg.contains("version") && cfg.version != "" { cfg.version } else { extract_version(image) };
    let container_port = if cfg.contains("container_port") { cfg.container_port } else { 3000 };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    let health_attempts = if cfg.contains("health_attempts") { cfg.health_attempts } else { 30 };
    let health_interval = if cfg.contains("health_interval") { cfg.health_interval } else { 2 };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    let pre_deploy_cmd = if cfg.contains("pre_deploy_cmd") { cfg.pre_deploy_cmd } else { "" };
    let proxy_kind = if cfg.contains("proxy") { cfg.proxy } else { "kamal" };
    let domain = if cfg.contains("domain") { cfg.domain } else { "" };

    // Pre-deploy hook (sequential ssh_exec, not fan-out).
    if pre_deploy_cmd != "" {
        print("    pre-deploy: " + pre_deploy_cmd);
        let pr = ssh_exec(host, pre_deploy_cmd);
        if !pr.ok {
            throw "Pre-deploy command failed on " + host + ":\n" + pr.stderr;
        }
    }

    // (a) Capture the OLD proxy target BY VALUE, BEFORE switching. The old
    //     container keeps running on its previous host port, so restoring the
    //     proxy to this value rolls traffic straight back.
    let old_target = if has_state(service + ".target." + host) {
        state_get(service + ".target." + host)
    } else if has_state(service + ".port." + host) {
        "localhost:" + state_get(service + ".port." + host)
    } else {
        "localhost:" + container_port
    };

    // Pick a free host port for the NEW container (deterministic symbolic port
    // under dry-run; real `nc -z` scan when live). The picked port is free, so it
    // also makes a collision-proof UNIQUE container name (works on macOS/BSD where
    // `date +%s%N` doesn't) that coexists with the still-running old container.
    let new_host_port = sim_pick_port(host, container_port);
    let new_name = service + "-web-" + version + "-" + new_host_port;
    let port_label = if is_dry_run() { "<auto>" } else { "" + new_host_port };

    // Build ONE proxy config map and use it IDENTICALLY for the forward switch AND the restore
    // compensation (issue #7/#23). Previously the restore omitted health_path, so the kamal
    // backend health-gated the restore against the default `/up`; for an app whose real health
    // path isn't `/up` the restore failed its gate, the compensation threw, and the unwind went
    // on to `rm -f` the new container — blackholing traffic. Same map both ways = no divergence.
    let proxy_cfg = #{ health_path: health_path, domain: domain };

    // Captured-by-value handles for the compensations.
    let svc = service;
    let h = host;
    let ot = old_target;
    let nn = new_name;
    let pk = proxy_kind;
    let pcfg = proxy_cfg;

    // (b) Start the NEW container under its unique name.
    print("    starting " + new_name + " on port " + port_label);
    let pmap = #{};
    pmap[new_host_port.to_string()] = container_port.to_string();
    let r = docker::docker_run(host, image, new_name, #{
        ports: pmap,
        envs: envs,
        volumes: volumes,
        network: network,
    });
    if !r.ok {
        throw "Failed to start " + new_name + " on " + host + ":\n" + r.stderr;
    }

    // Register the rm-new compensation IMMEDIATELY after the container starts —
    // BEFORE the health wait — so a health-check failure (the most common failure
    // mode) still tears the new container down on unwind (register-inverse-before-
    // effect, spec §7.1). It runs LAST in the LIFO unwind (after restore-proxy).
    on_rollback(|| {
        // Idempotent rm -f never errors if the container is already gone.
        docker::docker_remove(h, nn);
    });

    // (c) Wait until the NEW container answers HTTP health on its new port, BEFORE
    //     switching traffic. (HTTP only — matching the legacy deploy; we do NOT
    //     require a Docker HEALTHCHECK, which many images don't define.)
    print("    waiting for health...");
    let health_url = "http://" + host + ":" + new_host_port + health_path;
    health::wait_healthy(health_url, #{
        attempts: health_attempts,
        interval: health_interval,
    });

    // (d) Register the restore-proxy compensation BEFORE switching. It runs FIRST
    //     in the LIFO unwind, so on rollback traffic flows back to the still-running
    //     old container BEFORE the new one is removed (no blackhole window).
    on_rollback(|| {
        px_deploy(pk, h, svc, ot, pcfg);
    });

    // (e) Switch proxy traffic to the NEW container.
    let new_target = "localhost:" + new_host_port;
    print("    switching traffic -> " + new_target);
    px_deploy(proxy_kind, host, service, new_target, proxy_cfg);

    // (f) The OLD container is LEFT RUNNING under `canonical` (svc-web) so the
    //     restore-proxy compensation can route straight back to it. The post-commit
    //     pass retires it. We intentionally do NOT touch `canonical` here, and we do
    //     NOT persist the new port here (that happens post-commit, so a mid-fleet
    //     failure can't leave a stale port that corrupts the next deploy's old_target).
    #{ host: host, new_name: new_name, new_port: new_host_port }
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

// rollback(hosts, service, cfg) — roll the fleet back to a previous image.
//   cfg: #{ image: "" }   (empty -> use the snapshotted <service>.prev)
// Re-runs deploy() with the rollback image AND the FULL persisted deploy config
// (envs/port/health_path/volumes/network/proxy/domain), so the rolled-back
// container boots exactly like the deploy it's reverting to — not with defaults
// (issue #6). Any keys passed in `cfg` override the persisted config. build/push
// are always skipped (the image already exists). If no config was persisted (an
// older deploy), we fall back to defaults and warn.
fn rollback(hosts, service, cfg) {
    let image = if cfg.contains("image") { cfg.image } else { "" };
    if image == "" {
        if has_state(service + ".prev") {
            image = state_get(service + ".prev");
        } else {
            throw "No rollback image found for " + service + ". Specify one explicitly (cfg.image).";
        }
    }

    print("==> Rolling back " + service + " to " + image);

    // Start from the persisted effective config so the rollback replays the real deploy shape.
    let replay = #{};
    if has_state(service + ".config") {
        replay = from_json(state_get(service + ".config"));
    } else {
        print("  [warn] no persisted config for " + service + " — rolling back with DEFAULTS \
               (port/health/envs may be wrong). Re-deploy once to record a config.");
    }
    // Caller overrides win over the persisted config.
    for k in cfg.keys() {
        replay[k] = cfg[k];
    }
    // The image already exists in the registry/hosts; never rebuild/repush on rollback.
    replay.skip_build = true;
    replay.skip_push = true;
    // Don't re-run release tasks (migrations) on a rollback by default — rolling the SCHEMA back
    // is a separate, deliberate action. Drop pre_deploy unless the caller explicitly set it.
    if !cfg.contains("pre_deploy") {
        replay.pre_deploy = "";
    }

    // Save the current image as the NEXT rollback target before overwriting.
    if has_state(service + ".image") {
        let current = state_get(service + ".image");
        if current != () && current != image {
            state_set(service + ".prev", current);
        }
    }

    deploy(hosts, image, service, replay);
}

// rollback(hosts, service) — 2-arg overload (use the snapshotted prev image).
fn rollback(hosts, service) {
    rollback(hosts, service, #{})
}

// ---------------------------------------------------------------------------
// Accessories (databases, redis, etc. — long-lived, NOT part of the roll)
// ---------------------------------------------------------------------------

// accessory_run(host, name, image, cfg) — start a long-lived accessory container
// if it isn't already running.
//   cfg: #{ ports: #{}, envs: #{}, volumes: #{}, network: "", cmd: "" }
fn accessory_run(host, name, image, cfg) {
    // Sim-routed running check (honest read under dry-run).
    if docker::docker_container_running(host, name) {
        print("  accessory " + name + " already running on " + host);
        return;
    }

    print("  starting accessory " + name + " on " + host);
    let ports = if cfg.contains("ports") { cfg.ports } else { #{} };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    let cmd = if cfg.contains("cmd") { cfg.cmd } else { "" };

    let r = docker::docker_run(host, image, name, #{
        ports: ports,
        envs: envs,
        volumes: volumes,
        network: network,
        extra: cmd,
    });
    if !r.ok {
        throw "Failed to start accessory " + name + " on " + host + ":\n" + r.stderr;
    }
}

// accessory_run(host, name, image) — 3-arg overload (defaults).
fn accessory_run(host, name, image) {
    accessory_run(host, name, image, #{})
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

// extract_version(image) — last `:`-segment of an image ref, else "latest".
private fn extract_version(image) {
    if image.contains(":") {
        let parts = image.split(":");
        parts[parts.len() - 1]
    } else {
        "latest"
    }
}

// timestamp() — UTC timestamp string. local_exec is MUTATING-class, so under
// --dry-run this records the action and returns synthetic empty stdout; the
// deployed_at value is then empty in a plan. Acceptable (plan-only).
private fn timestamp() {
    let r = local_exec("date -u '+%Y-%m-%d %H:%M:%S UTC'");
    let out = r.stdout;
    out.trim();
    out
}
