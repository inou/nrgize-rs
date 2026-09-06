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
// choice is shared across these fresh module instances via the ephemeral,
// per-run session store (see lib/runtime.rhai), not module globals — it is
// NOT the durable StateStore, so it never leaks into a later run (robustness
// review R27).

import "lib/docker" as docker;
import "lib/proxy" as kproxy;     // kamal-proxy (the default)
import "lib/caddy" as cproxy;     // Caddy (selected with cfg.proxy == "caddy")
import "lib/healthcheck" as health;
import "lib/runtime" as rt;

// state_set / state_get / has_state / transaction / on_rollback are GLOBAL
// builtins (no import).

// Proxy dispatch — the fleet deploy is proxy-agnostic. `cfg.proxy` selects the backend:
// "kamal" (default, lib/proxy.rhai) or "caddy" (lib/caddy.rhai). Both expose the same
// proxy_boot / proxy_deploy surface, so a Caddy module (or your own) drops right in. `cfg` is
// forwarded whole to proxy_boot — `cfg.proxy_version` pins the proxy container's own image tag
// (Fable review, full-project pass: it used to always float "latest"/"2" with no way for a
// deploy() caller to pin it); any other keys `proxy_boot` doesn't read are simply ignored.
fn px_boot(kind, host, cfg) {
    if kind == "caddy" { cproxy::proxy_boot(host, cfg); } else { kproxy::proxy_boot(host, cfg); }
}
fn px_deploy(kind, host, service, target, cfg) {
    if kind == "caddy" {
        cproxy::proxy_deploy(host, service, target, cfg)
    } else {
        // Robustness review R12 follow-up (found during that fix's own Fable review):
        // `cfg.domain` reaches Caddy's route for automatic HTTPS (`lib/caddy.rhai`), but
        // kamal-proxy's own `proxy_deploy` (`lib/proxy.rhai`) never reads `cfg.domain` at
        // all — so a caller who sets `domain` while on the kamal-proxy backend (the
        // default) used to get it silently dropped, with no TLS/host routing and no
        // error, discoverable only by noticing the deployed service never got a
        // certificate. Fail loud instead: a `domain` this codebase can't actually honor on
        // the selected backend must be a clear error, not a silent no-op, matching the
        // established "don't silently drop a cfg key" direction (R20/R23c).
        if cfg.contains("domain") && cfg.domain != "" {
            throw "cfg.domain (" + cfg.domain + ") is set, but the kamal-proxy backend " +
                "(cfg.proxy: \"kamal\", the default) does not support domain-based routing " +
                "or automatic TLS in this codebase — use cfg.proxy: \"caddy\" instead, or " +
                "omit cfg.domain and handle TLS/routing outside nrg.";
        }
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
//     health_consecutive: 1,       // consecutive passing checks required (robustness review R12)
//     health_timeout: 30,          // per-request HTTP timeout in seconds (robustness review R12)
//     build_context: ".", dockerfile: "Dockerfile", build_args: #{},
//     skip_build: false, skip_push: false,
//     network: "",
//     publish_all_interfaces: false, // the new container's auto-picked host port is published on
//                                  // LOOPBACK ONLY (`-p 127.0.0.1:<port>:<container_port>`) by
//                                  // default — see deploy_one_host. Set true to publish it on
//                                  // every interface (0.0.0.0) instead, which also exposes the
//                                  // app to anyone who can reach the host, bypassing the proxy.
//     proxy: "kamal",               // "kamal" (default) or "caddy" — selects the backend
//     domain: "",                  // Caddy-only: a non-empty domain adds a host match ->
//                                  // automatic HTTPS. kamal-proxy does not support this in
//                                  // this codebase — setting `domain` with the default
//                                  // (kamal) backend THROWS rather than silently dropping it
//                                  // (robustness review, "kamal-proxy silently ignored
//                                  // cfg.domain", found during R12's review).
//     pre_deploy_cmd: "", post_deploy_cmd: "",  // TRUSTED-INPUT-ONLY raw shell (robustness
//                                  // review R28): run verbatim via ssh_exec, with NO
//                                  // sh_quote()/escaping applied — unlike every OTHER cfg
//                                  // field, which the stdlib quotes for you. Never build these
//                                  // strings from untrusted input (see docs/safety.md,
//                                  // "Escape hatches: trusted-input-only raw shell").
//     // keep_images: unset,       // tagged-image retention, opt-in (robustness review R22):
//                                  // N >= 0 prunes image_repo's OTHER tags on each host beyond
//                                  // the N most recent, always protecting the current and
//                                  // (if any) previous version. Omit entirely to disable.
//     skip_lock: false,            // opt OUT of the cross-machine deploy lock (robustness
//                                  // review R15) — the lock is on by default.
//   }
//
// Throws if any host fails; attempts best-effort fleet compensation before re-raising.
fn deploy(hosts, image, service, cfg) {
    // REFUSE to run nested inside an already-active transaction() (robustness review R29,
    // found during R6's review). deploy() wraps its own transaction() around the rolling loop,
    // then runs POST-COMMIT cleanup (rename/stop/remove the old container) right after that
    // transaction returns Ok, treating the commit as final. But a nested transaction's
    // compensations are deliberately kept alive on the stack for an ENCLOSING transaction's
    // later unwind (see docs/safety.md, "Nesting") — so if this deploy() were called from
    // inside a caller's own transaction() (e.g. wrapping several deploy() calls for a
    // multi-service atomic release), an unrelated LATER failure in that outer transaction would
    // resurrect this deploy()'s already-superseded per-host compensations, acting on state
    // (the old container) that post-commit already removed — a real outage. Checked BEFORE any
    // build/push/pull work, matching the arch-mismatch preflight's fail-fast philosophy.
    if in_transaction() {
        throw "deploy() cannot be called from inside an active transaction() — nesting it would " +
              "let an unrelated later failure resurrect this deploy's already-committed rollback " +
              "compensations against state that no longer matches them, causing a real outage " +
              "(robustness review R29). Call deploy() at the top level for each service instead.";
    }

    // Robustness review R21: an empty `hosts` array used to either panic (an out-of-bounds
    // `hosts[0]` below, reached whenever `cfg.pre_deploy`/arch-checking touched it first) or,
    // with neither of those in play, silently "succeed" having touched NO host at all while
    // still persisting new `.version`/`.image`/`.prev` state below — claiming a release that
    // never actually happened anywhere, and repointing the rollback chain at a version nothing
    // is running. Checked here, before any of that, matching every other precondition above.
    if hosts.is_empty() {
        throw "deploy() called with an empty hosts array for '" + service + "' — nothing would " +
              "actually be deployed, but state would still be persisted as if it had been " +
              "(robustness review R21). Pass at least one host.";
    }

    let version = extract_version(image);

    let container_port = if cfg.contains("container_port") { cfg.container_port } else { 3000 };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    let health_attempts = if cfg.contains("health_attempts") { cfg.health_attempts } else { 30 };
    let health_interval = if cfg.contains("health_interval") { cfg.health_interval } else { 2 };
    // Robustness review R12: a single 200 used to pass the gate — an app that answers /up once
    // during boot then crashes still got traffic switched to it. health_consecutive requires that
    // many CONSECUTIVE passing checks before deploy() considers the new container healthy.
    let health_consecutive = if cfg.contains("health_consecutive") { cfg.health_consecutive } else { 1 };
    // Robustness review R12: each health-check HTTP request used to have a hardcoded 30s timeout
    // unrelated to health_interval — a hanging endpoint made `attempts: 30` (intended as roughly a
    // 1-minute budget at the default 2s interval) actually take up to 30 * 30s = 15 minutes.
    let health_timeout = if cfg.contains("health_timeout") { cfg.health_timeout } else { 30 };
    let build_context = if cfg.contains("build_context") { cfg.build_context } else { "." };
    let dockerfile = if cfg.contains("dockerfile") { cfg.dockerfile } else { "Dockerfile" };
    let build_args = if cfg.contains("build_args") { cfg.build_args } else { #{} };
    // A single target platform (e.g. "linux/amd64"), or a comma-separated list (e.g.
    // "linux/amd64,linux/arm64") for a multi-platform manifest-list build — see docker_build's
    // own doc comment in lib/docker.rhai. Empty means "build for whatever architecture this
    // machine is" (the historical default).
    let platform = if cfg.contains("platform") { cfg.platform } else { "" };
    // Roadmap 1.1 step 3a: build on a remote host over SSH instead of this machine (e.g. a
    // native arm64 builder, so an arm64 target needs no buildx/qemu emulation at all) — see
    // docker_build's own doc comment in lib/docker.rhai. Empty (default) is the existing local
    // build, unchanged.
    let build_host = if cfg.contains("build_host") { cfg.build_host } else { "" };
    let skip_build = if cfg.contains("skip_build") { cfg.skip_build } else { false };
    let skip_push = if cfg.contains("skip_push") { cfg.skip_push } else { false };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    // The auto-picked host port of each new container is published on LOOPBACK ONLY by default
    // (see deploy_one_host for the full reasoning). Opt-in escape hatch for the rare host that
    // really does need the container reachable from off-box; forwarded to deploy_one_host below.
    let publish_all_interfaces = if cfg.contains("publish_all_interfaces") { cfg.publish_all_interfaces } else { false };
    // pre_deploy: an IN-CONTAINER release command (e.g. "bin/rails db:migrate") run ONCE from a
    // throwaway container built on the NEW image, BEFORE the roll (issue #8). pre_deploy_cmd is
    // the legacy per-host RAW host command (kept for non-migration hooks); prefer pre_deploy for
    // anything that must run against the new image's code.
    let pre_deploy = if cfg.contains("pre_deploy") { cfg.pre_deploy } else { "" };
    let pre_deploy_cmd = if cfg.contains("pre_deploy_cmd") { cfg.pre_deploy_cmd } else { "" };
    let post_deploy_cmd = if cfg.contains("post_deploy_cmd") { cfg.post_deploy_cmd } else { "" };
    let proxy_kind = if cfg.contains("proxy") { cfg.proxy } else { "kamal" };
    let domain = if cfg.contains("domain") { cfg.domain } else { "" };
    // "kamal-proxy silently ignored cfg.domain" (found during R12's own Fable review): a
    // `domain` + kamal-proxy combination is a purely static config error, knowable before doing
    // any work at all — checked here, matching the R29/R21/arch-mismatch fail-fast checks above,
    // rather than only in `px_deploy` below (which wouldn't throw until AFTER build, push,
    // pre_deploy migrations against the new image, and the first host's container start/health
    // wait had already run). `px_deploy`'s own check stays too, as defense-in-depth: `px_deploy`
    // is itself a public function (unlike the `private fn deploy_one_host` it's called from), so
    // a script that imports this module and calls `deploy::px_deploy(...)` directly bypasses this
    // fail-fast check entirely — but this is what actually fires for an ordinary `deploy()` call.
    if proxy_kind != "caddy" && domain != "" {
        throw "cfg.domain (" + domain + ") is set, but the kamal-proxy backend (cfg.proxy: " +
            "\"kamal\", the default) does not support domain-based routing or automatic TLS " +
            "in this codebase — use cfg.proxy: \"caddy\" instead, or omit cfg.domain and " +
            "handle TLS/routing outside nrg.";
    }
    // Fable final review (roadmap 1.1 step 3b): a comma-separated cfg.platform is a
    // multi-platform manifest-list build, and `docker_build` publishes it via `buildx build
    // --push` — there is no `--load`-only variant for more than one platform, so `cfg.skip_push`
    // cannot actually be honored here: the registry gets written during the build regardless of
    // what the caller asked for. Silently ignoring an explicit `skip_push: true` would violate
    // the caller's instruction, so fail fast instead — same fail-fast-on-a-static-cfg-error
    // pattern as the domain/proxy check just above. Only applies when a build will actually run
    // this call (`!skip_build`); with `skip_build: true` there's no build to violate skip_push.
    if !skip_build && skip_push && platform.contains(",") {
        throw "cfg.skip_push is set, but cfg.platform (" + platform + ") is a comma-separated " +
            "multi-platform manifest-list build — buildx can only publish a manifest list via " +
            "`--push` at build time (there's no `--load`-only variant for more than one " +
            "platform), so the registry write can't be skipped here. Omit cfg.skip_push, or " +
            "build a single platform instead if you need a local-only, unpushed image.";
    }
    // Robustness review R22: `docker_cleanup`'s prune only ever removed DANGLING images —
    // `image_repo`'s own old TAGGED versions accumulated on every host forever, until the disk
    // filled. Strictly opt-in (unset means "unchanged behavior, no tagged-image pruning at all"):
    // an operator must explicitly ask for this, since it's a destructive disk-freeing operation.
    let keep_images = if cfg.contains("keep_images") { cfg.keep_images } else { -1 };
    if cfg.contains("keep_images") && keep_images < 0 {
        throw "deploy() called with a negative cfg.keep_images (" + keep_images + ") for '" +
              service + "' — must be >= 0 (0 keeps only the current and, if any, previous " +
              "version; omit the key entirely to disable tagged-image pruning).";
    }
    // Robustness review R15: no guard against a SECOND, concurrent deploy of the SAME service —
    // the existing project-level flock (docs/safety.md) only serializes runs on ONE control
    // machine; a teammate's laptop or a second CI runner racing this one could both "successfully"
    // pick the same free host port (a late failure then unwinds an otherwise-healthy fleet) or
    // interleave the canonical rename dance, corrupting `.target.<host>` state for every deploy
    // after. Strictly opt-out (`cfg.skip_lock`), not opt-in — the whole point is protection an
    // operator shouldn't have to remember to ask for — since the lock depends on remote
    // infrastructure (a writable /tmp, a POSIX shell) this codebase can't unconditionally
    // guarantee for every exotic host, unlike the pure-Rhai R21/R29 guards.
    let skip_lock = if cfg.contains("skip_lock") { cfg.skip_lock } else { false };
    let lock_hosts = hosts;
    lock_hosts.sort();
    lock_hosts.dedup();
    let lock_dir = "/tmp/nrg-deploy-lock-" + service;
    if !skip_lock {
        for lock_host in lock_hosts { acquire_deploy_lock(lock_host, lock_dir, service); }
    }

    print("==> Deploying " + service + " " + version + " to " + hosts.len() + " host(s)");
    // The rest of deploy()'s real work runs inside this try so ANY failure — build, push, pull,
    // pre_deploy, an unwound transaction, whatever — releases the lock before re-raising, instead
    // of leaving it held after a failed deploy blocks every future one. NOTE: this can't catch a
    // genuine SIGINT/SIGTERM — this engine's interrupt handling (robustness review R7) makes that
    // an `ErrorTerminated` that deliberately bypasses script-level `try`/`catch` (the same reason
    // `transaction()`'s own unwind relies on a Rust-level mechanism, not Rhai `catch`) — so a
    // Ctrl-C'd deploy leaves the lock held, same known limitation as the local flock's own
    // documented `NRG_STATE_LOCK` staleness gap. No automatic TTL by design: a timeout short
    // enough to matter risks letting two deploys run concurrently anyway on a slow-but-healthy
    // one, which is worse than an occasional manual `rm -rf` (the error message below names the
    // exact command).
    try {
    // Roadmap 2.6: an optional, orchestration-file-defined `hook_pre_deploy(service, image,
    // hosts)` — called FIRST, before any build/push/pull/pre_deploy work, so it can genuinely
    // BLOCK the deploy by throwing (e.g. "not during the freeze window") with nothing yet done.
    // Unlike the post-deploy/post-rollback hooks below, this one is NOT wrapped in its own
    // try/catch — a throw here is a real, intentional abort, and the existing catch just below
    // already releases the lock before re-raising, exactly like any other early failure in this
    // function.
    //
    // Fable final review: rollback() calls THIS function, and rollback() overwrites `.prev`
    // BEFORE calling deploy() — so if this were the only place the hook fired, a blocking
    // hook_pre_deploy would corrupt the rollback chain (`.prev` already clobbered by the time
    // the hook throws). rollback() now calls call_pre_deploy_hook itself, before that mutation,
    // and sets `cfg.__skip_pre_deploy_hook` on the replay it passes down so it isn't called a
    // second time here for the same rollback.
    let skip_pre_deploy_hook = if cfg.contains("__skip_pre_deploy_hook") { cfg.__skip_pre_deploy_hook } else { false };
    if !skip_pre_deploy_hook {
        call_pre_deploy_hook(service, image, hosts);
    }

    // Robustness review R10: ":latest" (or no tag at all, which Docker treats identically) is a
    // MUTABLE registry pointer. If this build turns out to be broken, rollback() may not be able
    // to safely undo it — the registry tag can already point at this SAME broken build by the
    // time a rollback runs, silently turning "rollback" into a no-op redeploy of the broken image.
    // A soft warning, not a hard refusal: ":latest" is the documented quickstart default (see
    // docs/getting-started.md), so blocking it outright would break that flow. rollback() itself
    // (below) refuses outright when it would actually act on a mutable snapshot. Case-insensitive
    // (`.to_lower()`): Docker's own tag charset allows uppercase (e.g. a tag literally named
    // "LATEST" is syntactically valid and distinct from "latest"), so a CI script or operator
    // typo using a different case must still be caught — this is the same mutable-tag risk, just
    // spelled differently.
    if version.to_lower() == "latest" {
        print(
            "  [warn] deploying a mutable \":latest\" tag for " + service + " — if this build turns " +
            "out to be broken, rollback() may not be able to safely undo it, since the registry tag " +
            "may already point at this same broken build by the time you roll back (robustness " +
            "review R10). Prefer an immutable tag (e.g. a git SHA) for anything you might need to " +
            "roll back."
        );
    }

    // -------------------------------------------------------------------
    // Phase 1-2: Build & push (OUTSIDE the transaction — no rollback).
    // -------------------------------------------------------------------
    if !skip_build {
        // Catch a build-machine-vs-deploy-target ARCHITECTURE mismatch here, before spending
        // time on build/push/pull, instead of at container start on the host (an opaque
        // exec-format error, AFTER build/push/pull have all already appeared to succeed). Only
        // when the caller hasn't already told us which platform to target — an explicit
        // `platform` is an intentional cross-arch build, not a mismatch to warn about.
        if platform == "" {
            check_arch_mismatch(build_host, hosts[0]);
        }
        print("\n--- Build ---");
        docker::docker_build(image, #{
            context: build_context,
            dockerfile: dockerfile,
            build_args: build_args,
            platform: platform,
            build_host: build_host,
        });
    }
    // A comma-separated `platform` (roadmap 1.1 step 3b) is a multi-platform manifest-list
    // build — docker_build already pushed it directly via `buildx build --push`, so there is
    // nothing local left for a separate `docker push` to do (buildx --push never loads a local
    // image for that tag, so a plain push here would either fail outright or silently push a
    // stale single-arch image left over from an earlier build under the same tag).
    let is_multi_platform_build = platform.contains(",");
    if !skip_push && !is_multi_platform_build {
        print("\n--- Push ---");
        // Roadmap 1.1 step 3a: when cfg.build_host is set, the image was built THERE and only
        // exists there — pushing from this machine instead would either push nothing (image not
        // found) or a stale image left over from an earlier LOCAL build under the same tag.
        docker::docker_push(build_host, image);
    } else if !skip_push && is_multi_platform_build {
        if skip_build {
            // Opus review: `docker_build` never ran this call (skip_build), so buildx never
            // pushed anything — don't claim it did.
            print("\n--- Push --- (skipped: cfg.skip_build is set, so there is no build here " +
                  "that could have pushed the manifest list — nothing to push)");
        } else {
            print("\n--- Push --- (skipped: buildx already pushed the manifest list during build)");
        }
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
        px_boot(proxy_kind, host, cfg);
    }

    // -------------------------------------------------------------------
    // Read the predecessor without changing rollback history. It is committed
    // atomically with the successful release after all traffic switches.
    // -------------------------------------------------------------------
    let prev_image = if has_state(service + ".image") { state_get(service + ".image") } else { "" };


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
                health_consecutive: health_consecutive,
                health_timeout: health_timeout,
                network: network,
                publish_all_interfaces: publish_all_interfaces,
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
    let effective_cfg = #{
        container_port: container_port,
        envs: envs,
        volumes: volumes,
        health_path: health_path,
        health_attempts: health_attempts,
        health_interval: health_interval,
        health_consecutive: health_consecutive,
        health_timeout: health_timeout,
        network: network,
        proxy: proxy_kind,
        domain: domain,
        pre_deploy: pre_deploy,
        pre_deploy_cmd: pre_deploy_cmd,
        post_deploy_cmd: post_deploy_cmd,
    };
    // Robustness review R22: unlike skip_build/skip_push/build_context/dockerfile/build_args/
    // platform (all forced to build-nothing on rollback replay regardless, so persisting them
    // would be meaningless), keep_images is a disk-hygiene behavior toggle a caller who set it
    // once should keep getting on every replayed rollback — added ONLY when the caller actually
    // set it (never the -1 "unset" sentinel): the sentinel doubles as an explicit-negative-value
    // validation trigger above, so persisting it unconditionally would make EVERY future
    // rollback() replay hit that same "negative cfg.keep_images" throw, permanently breaking
    // rollback for any service that has ever deployed without this key set.
    if cfg.contains("keep_images") {
        effective_cfg.keep_images = keep_images;
    }
    // Same "only when the caller actually set it" treatment as keep_images just above, for the
    // same reason in reverse: a service deployed with `publish_all_interfaces: true` must keep
    // that bind on a replayed rollback (issue #6 — a rollback must boot the container the way the
    // deploy it reverts to booted it, not with defaults), while a service that never set the key
    // keeps its persisted config byte-identical to before this key existed.
    if cfg.contains("publish_all_interfaces") {
        effective_cfg.publish_all_interfaces = publish_all_interfaces;
    }
    // Robustness review R24: `effective_cfg.envs` may contain plaintext values from
    // reveal()'d secrets (needed so rollback() can replay the SAME env vars into a real
    // redeploy — see rollback()'s "replay = from_json(state_get(service + \".config\"))"
    // below). This is a deliberate design tradeoff, not an oversight: `.energize/state.json`
    // is written 0600 (see StateStore::flush), but that only mitigates LOCAL exposure — a
    // workspace archive, CI artifact upload, or a state backup can still exfiltrate it. See
    // docs/safety.md, "Deploy state may contain secret plaintext", for the full contract and
    // what operators must do (never commit/archive/upload `.energize/` unprotected).
    let release = #{};
    if prev_image != "" && prev_image != image {
        release[service + ".prev"] = prev_image;
        if has_state(service + ".config") { release[service + ".prev_config"] = state_get(service + ".config"); }
    }
    release[service + ".runtime.cmd"] = rt::container_cmd();
    release[service + ".runtime.name"] = rt::runtime_name();
    release[service + ".config"] = to_json(effective_cfg);
    release[service + ".version"] = version;
    release[service + ".image"] = image;
    release[service + ".deployed_at"] = timestamp();
    // Robustness review R27 (found during its own review): re-persist the runtime THIS deploy
    // actually resolved to (via the ephemeral session store, see lib/runtime.rhai) on every
    // successful deploy — not just when the script happens to call set_runtime() explicitly.
    // Without this, editing a script to remove a `set_runtime("podman")` call correctly makes
    // the NEXT deploy use docker (the R27 fix), but the durable mirror `nrg status`/`nrg logs`/
    // `nrg app exec` read would keep saying "podman" forever, since nothing else ever
    // overwrites it — silently misleading those commands about a runtime this service hasn't
    // used since. Writing it here keeps the durable copy honest: it always reflects the last
    // ACTUAL deploy's runtime, not just the last explicit `set_runtime()` call.
    release["nrg.runtime.cmd"] = rt::container_cmd();
    release["nrg.runtime.name"] = rt::runtime_name();

    for entry in rolled {
        release[service + ".port." + entry.host] = "" + entry.new_port;
        release[service + ".target." + entry.host] = "localhost:" + entry.new_port;
    }
    state_update(release);

    print("\n--- Post-commit cleanup ---");
    let canonical = service + "-web";
    let old_name = service + "-web-old";
    // Robustness review R22: which tags of THIS repo must survive pruning no matter how old they
    // are — the version just deployed, plus the previous version rollback() might still need
    // (only if `prev_image` is actually the SAME repo; a caller who changes `image_repo` between
    // deploys has an unrelated old repo in `.prev`, irrelevant to pruning this one).
    let image_repo = extract_repo(image);
    let protect_tags = [version];
    if prev_image != "" && extract_repo(prev_image) == image_repo {
        protect_tags.push(extract_version(prev_image));
    }
    for entry in rolled {
        let host = entry.host;
        let new_name = entry.new_name;
        let new_port = entry.new_port;
        // The old canonical container is still running; retire it, promote the new.
        // `docker_stop`/`docker_remove`/`docker_cleanup` still bake `|| true` into the REMOTE
        // shell command (by design — idempotent, so a partial retry never wedges on a step
        // that's already done). That masks a docker-level failure, but NOT an ssh-level one: if
        // the SSH command itself never reaches the host (connection dropped, auth failure), the
        // remote shell — and its `|| true` — never runs at all, and the returned ExecResult's
        // `ok` is false. `docker_rename` (the two calls below) is different since the M6 fix
        // (Fable review, full-project pass): it no longer relies on a blanket `|| true` at all —
        // it classifies the docker-level result itself, downgrading only the idempotent "already
        // renamed away" case to success and surfacing everything else (a real name conflict,
        // permission denied, ...) as a genuine failure. Robustness review R6b: before ITS fix,
        // an SSH-level failure here used to be silently discarded, so a dropped connection left
        // the OLD container still running under `--restart unless-stopped` (untouched) and the
        // NEW container never promoted to its canonical name, while `.port`/`.target` state was
        // persisted right below as if the swap had completed — a permanent mismatch between
        // recorded state and host reality that compounds on every subsequent deploy's rename
        // dance. Both fixes land in the SAME `!r.ok` check below.

        let r1 = docker::docker_rename(host, canonical, old_name);
        if !r1.ok { print("[warn] active route saved; rename requires reconciliation: " + r1.stderr); continue; }
        let r2 = docker::docker_promote(host, new_name, canonical);
        if !r2.ok { print("[warn] active route saved; promotion requires reconciliation: " + r2.stderr); continue; }
        let r3 = docker::docker_stop(host, old_name);
        let r4 = docker::docker_remove(host, old_name);

        let failed = [];
        if !r1.ok { failed.push("rename " + canonical + " -> " + old_name + ": " + r1.stderr); }
        if !r2.ok { failed.push("rename " + new_name + " -> " + canonical + ": " + r2.stderr); }
        if !r3.ok { failed.push("stop " + old_name + ": " + r3.stderr); }
        if !r4.ok { failed.push("remove " + old_name + ": " + r4.stderr); }

        if !failed.is_empty() {
            print(
                "  [warn] post-commit cleanup on " + host + " may not have completed — an SSH " +
                "command failed to reach the host (e.g. a dropped connection), so the OLD and " +
                "NEW containers there may BOTH still be running under their pre-swap names " +
                "(wasted capacity, possibly stale code still being served). This host's " +
                "active port/target were saved; the transition journal remains pending. " +
                "Failed step(s):\n    " +
                join(failed, "\n    ") + "\n  Verify " + host + " manually before the next deploy."
            );
            continue;
        }
        state_del(service + ".transition." + host);
        // Robustness review R22: opt-in tagged-image retention, deliberately NOT part of the
        // `failed` gate above — a pruning failure (or the feature being off entirely) has no
        // bearing on whether the swap itself completed, so it must never block persisting the
        // new port/target below. Best-effort: reported as its own separate [warn] if the listing
        // step failed, never thrown.
        if keep_images >= 0 {
            let pr = docker::docker_prune_old_images(host, image_repo, keep_images, protect_tags);
            if !pr.ok {
                print(
                    "  [warn] could not list " + image_repo + " images on " + host + " to prune " +
                    "old tagged versions (robustness review R22) — skipped this round rather than " +
                    "act on incomplete data: " + pr.stderr
                );
            } else if !pr.removed.is_empty() {
                print("  pruned old tagged image(s) on " + host + ": " + join(pr.removed, ", "));
            }
        }
        // Persist the new port + proxy target NOW (post-commit) — never inside the
        // transaction, so a mid-fleet failure can't leave a stale port behind.

    }

    // Post-deploy hook (best-effort, after the fleet is committed).
    if post_deploy_cmd != "" {
        run_post_deploy_hook(hosts, post_deploy_cmd);
    }

    // -------------------------------------------------------------------
    // Phase 7: Persist deploy state, INCLUDING the full effective config, so
    // rollback() can replay it verbatim instead of reverting envs/port/health/
    // proxy to defaults (issue #6). build/push are forced off on replay. The
    // config is JSON in state; on a LIVE run this is a 0600 file (see state.rs),
    // and the dry-run plan redacts any registered secret in the blob.
    // -------------------------------------------------------------------

    print("\n==> Deploy complete: " + service + " " + version);
    } catch (err) {
        if !skip_lock { for lock_host in lock_hosts { release_deploy_lock(lock_host, lock_dir, service); } }
        throw err;
    }
    if !skip_lock { for lock_host in lock_hosts { release_deploy_lock(lock_host, lock_dir, service); } }

    // Roadmap 2.6: an optional, orchestration-file-defined `hook_post_deploy(service, image,
    // hosts)` — called AFTER the lock is released, so a slow notification never extends how long
    // this deploy holds the cross-machine lock (robustness review R15). Best-effort by the same
    // convention run_post_deploy_hook already established above: the fleet has already committed,
    // so a hook that throws can't roll anything back — it's reported loudly, not swallowed
    // silently, but it never turns an already-successful deploy into a reported failure.
    call_post_deploy_hook(service, image, hosts);
}

// deploy(hosts, image, service) — 3-arg overload (defaults).
fn deploy(hosts, image, service) {
    deploy(hosts, image, service, #{})
}

// run_post_deploy_hook(hosts, cmd) -> Array — run `cmd` on every host via ssh_exec,
// best-effort. Returns the list of "<host>: <stderr>" strings for hosts where it failed (empty
// if all hosts succeeded) — so a caller (or a test) can inspect exactly what failed without
// having to scrape the printed warning.
//
// Robustness review R20: this used to discard ssh_exec's result entirely — a hook that failed
// on some (or all) hosts still reported the whole deploy as a full success, with no visible
// signal anything went wrong. Called AFTER the fleet has already committed (the whole point of
// "best-effort"), so a failure here can't roll anything back — but it must at least be reported
// loudly instead of silently swallowed. Pulled out of deploy() into its own function so it's
// directly unit-testable without needing a full live deploy to reach it. Not marked `private`
// (unlike deploy_one_host) — unlike that function, calling this standalone has no correctness
// hazard (it doesn't bypass any transaction/guard), so there's no reason to hide it.
// `cmd` is a TRUSTED-INPUT-ONLY raw-shell escape hatch (robustness review R28): run verbatim
// via ssh_exec on every host, with NO sh_quote()/escaping applied.
fn run_post_deploy_hook(hosts, cmd) {
    let failed = [];
    for host in hosts {
        print("    post-deploy on " + host + ": " + cmd);
        let r = ssh_exec(host, cmd);
        if !r.ok {
            failed.push(host + ": " + r.stderr);
        }
    }
    if !failed.is_empty() {
        print(
            "  [warn] post-deploy hook failed on: " + join(failed, "; ") + " (robustness " +
            "review R20) — the fleet traffic switch already committed and is NOT affected, " +
            "but this hook did not run successfully everywhere; verify manually."
        );
    }
    failed
}

// ---------------------------------------------------------------------------
// Lifecycle hooks (roadmap 2.6) — OPTIONAL Rhai functions the orchestration file may
// (but need not) define. Unlike cfg.pre_deploy (an in-container release command STRING run
// against the new image) and cfg.pre_deploy_cmd/post_deploy_cmd (TRUSTED-INPUT-ONLY raw host
// shell — robustness review R28), these are full Rhai FUNCTIONS the calling script defines by
// NAME, called back via Rhai's `is_def_fn`/`Fn(name).call(...)` — deliberately named
// `hook_*` (not `pre_deploy`/`post_deploy`) to avoid any confusion with those existing,
// unrelated cfg keys.
//
//   fn hook_pre_deploy(service, image, hosts) { ... }     // may THROW to block the deploy
//   fn hook_post_deploy(service, image, hosts) { ... }     // best-effort; a throw is only warned
//   fn hook_post_rollback(service, image, hosts) { ... }   // best-effort; a throw is only warned
//
// `is_def_fn(name, arity)` + `Fn(name).call(...)` see every function defined at the TOP LEVEL of
// the running script (the orchestration file), even when called from deep inside an imported
// module like this one — verified empirically before implementing this feature, since Rhai's
// function resolution is global-per-AST-execution, not lexically scoped to the importing file.
// A hook is looked up by exact name AND arity (3 positional args here) — an orchestration file
// that defines `fn hook_post_deploy(service)` (wrong arity) is treated the same as not having
// defined the hook at all, rather than erroring on a mismatched call.
//
// Opus review: `Fn(name).call(...)` resolves a MODULE-internal function of the same name+arity in
// PREFERENCE to the calling script's top-level one (verified empirically) — so `hook_pre_deploy`,
// `hook_post_deploy`, and `hook_post_rollback` are effectively RESERVED 3-arg names across this
// entire module and everything it imports (`docker`, `proxy`, `caddy`, `healthcheck`, `runtime`).
// None currently exists; never add an internal 3-arg function under any of these three names —
// doing so would make `is_def_fn` report every caller as "having defined" that hook (even one
// that defined nothing), silently invoking the wrong function.
private fn call_pre_deploy_hook(service, image, hosts) {
    if is_def_fn("hook_pre_deploy", 3) {
        Fn("hook_pre_deploy").call(service, image, hosts);
    }
}

private fn call_post_deploy_hook(service, image, hosts) {
    if is_def_fn("hook_post_deploy", 3) {
        try {
            Fn("hook_post_deploy").call(service, image, hosts);
        } catch (err) {
            print(
                "  [warn] hook_post_deploy threw: " + err + " — the deploy already succeeded " +
                "and is unaffected."
            );
        }
    }
}

private fn call_post_rollback_hook(service, image, hosts) {
    if is_def_fn("hook_post_rollback", 3) {
        try {
            Fn("hook_post_rollback").call(service, image, hosts);
        } catch (err) {
            print(
                "  [warn] hook_post_rollback threw: " + err + " — the rollback already " +
                "succeeded and is unaffected."
            );
        }
    }
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
//           health_attempts, health_interval, health_consecutive, health_timeout,
//           network, pre_deploy_cmd }
//
// Returns the unique NEW container name (so the caller can finish the swap).
// Throws on any per-host failure (which unwinds the enclosing transaction).
//
// INTERNAL — only ever called from deploy()'s own transaction(), never directly. It relies on
// deploy()'s R29 nesting guard having already run and on deploy()'s post-commit pass to retire
// the old container; calling it standalone bypasses both.
private fn deploy_one_host(host, image, service, cfg) {
    let version = if cfg.contains("version") && cfg.version != "" { cfg.version } else { extract_version(image) };
    let container_port = if cfg.contains("container_port") { cfg.container_port } else { 3000 };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let health_path = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    let health_attempts = if cfg.contains("health_attempts") { cfg.health_attempts } else { 30 };
    let health_interval = if cfg.contains("health_interval") { cfg.health_interval } else { 2 };
    let health_consecutive = if cfg.contains("health_consecutive") { cfg.health_consecutive } else { 1 };
    let health_timeout = if cfg.contains("health_timeout") { cfg.health_timeout } else { 30 };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    let publish_all_interfaces = if cfg.contains("publish_all_interfaces") { cfg.publish_all_interfaces } else { false };
    let pre_deploy_cmd = if cfg.contains("pre_deploy_cmd") { cfg.pre_deploy_cmd } else { "" };
    let proxy_kind = if cfg.contains("proxy") { cfg.proxy } else { "kamal" };
    let domain = if cfg.contains("domain") { cfg.domain } else { "" };

    // Pre-deploy hook (sequential ssh_exec, not fan-out). TRUSTED-INPUT-ONLY raw shell
    // (robustness review R28): run verbatim via ssh_exec, with NO sh_quote()/escaping applied.
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
    } else if docker::docker_container_running(host, service + "-web") {
        // Robustness review R4b: no `.target.<host>`/`.port.<host>` state exists (fresh CI
        // runner, unshared state, or a state.json reset), but a canonical OLD container is
        // ACTUALLY running here — this is NOT a fresh deploy, we've just lost track of where it's
        // listening. `sim_pick_port` hands out an essentially arbitrary free host port, so
        // guessing "localhost:<container_port>" (the IN-CONTAINER port) is almost certainly
        // wrong. A wrong guess isn't merely cosmetic: if THIS host's roll then fails and the fleet
        // unwinds, the "restore proxy" compensation would "succeed" pointing traffic at a target
        // nothing listens on, while the OTHER registered compensation still tears down the new
        // container — an outage the rollback itself caused, on a host that was serving traffic
        // just fine before this deploy attempt. Fail loudly instead of silently guessing.
        throw "Cannot determine " + service + "'s previous proxy target on " + host + ": a " +
              "container named '" + service + "-web' is already running there, but no " +
              "'" + service + ".target." + host + "' or '" + service + ".port." + host + "' state " +
              "is recorded (fresh runner? state.json reset or not shared with this host?) — " +
              "guessing would risk pointing a rollback's restored proxy target at nothing while " +
              "still removing the new container (robustness review R4b). Record the real host " +
              "port before retrying: state_set(\"" + service + ".port." + host + "\", <port>).";
    } else {
        // No recorded state AND no old container running here — genuinely nothing to roll back
        // to on this host (first deploy, or a newly added fleet member), so a guessed target
        // can't make anything worse than it already was.
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
    // NOT captured-by-value: these two are deliberately SHARED between the on_rollback closures
    // below (Rhai auto-promotes a variable referenced by more than one closure in the same scope
    // to a shared cell, so a write in one is visible from the other) — see the rm-new compensation
    // for why (robustness review R6). Two flags, not one: `proxy_switched` becomes true only once
    // the FORWARD switch (e) actually happens; `proxy_restored` becomes true only once the
    // restore compensation (d) succeeds. Removing the new container is unsafe ONLY when the proxy
    // was switched to it AND the restore back failed — if the proxy was never switched (e.g. the
    // health check below throws first), the old container is still serving and removing the new,
    // never-healthy one is exactly the correct cleanup, not a blackhole risk.
    let proxy_switched = false;
    let proxy_restored = false;

    // (b) Start the NEW container under its unique name.
    print("    starting " + new_name + " on port " + port_label);
    let pmap = #{};
    // Publish on LOOPBACK ONLY (127.0.0.1) by default. A bare "<port>" key renders as
    // `-p '<port>:<cport>'` (lib/docker.rhai), which makes Docker bind 0.0.0.0 — every deployed
    // app then answers plaintext HTTP at a predictable port (the scan window starts at
    // container_port + 10000) to anyone who can reach the host, bypassing the proxy that is the
    // actual enforcement point for TLS, `cfg.domain` host matching and `proxy_maintenance`'s 503.
    // A host firewall does NOT close that path either: Docker's published-port DNAT is evaluated
    // before the INPUT chain a ufw/firewalld rule lives in.
    //
    // Nothing in a deploy needs this port from OFF the host: both proxy backends run with
    // `--network host` (lib/proxy.rhai, lib/caddy.rhai) and dial the "localhost:<port>" target set
    // below, the health gate curls `http://localhost:<port>` FROM the host over SSH
    // (wait_healthy_on_host, R7-health), and `sim_pick_port`'s free-port scan is an on-host
    // `nc -z localhost <port>`.
    //
    // Deliberately done HERE and not in `docker_run`'s `-p` builder (which already passes an
    // IP-prefixed key straight through, so it needs no change): `accessory_run` shares that
    // builder, and an accessory database published on its own host is legitimately reached across
    // the fleet — accessory publishes stay exactly as the caller wrote them.
    let publish_key = if publish_all_interfaces {
        new_host_port.to_string()
    } else {
        "127.0.0.1:" + new_host_port
    };
    pmap[publish_key] = container_port.to_string();
    if has_state(service + ".transition." + host) {
        throw "Unreconciled deployment transition for " + service + " on " + host + "; verify remote routing and containers before clearing this journal.";
    }
    on_rollback(|| {
        if proxy_switched && !proxy_restored {
            print("  [rollback] proxy restore did not succeed on " + h + " — leaving " + nn
                + " running instead of removing it (removing it could blackhole traffic); "
                + "clean up manually once the proxy is confirmed pointed at a healthy container");
            return;
        }
        // Idempotent rm -f never errors if the container is already gone.
        let removed = docker::docker_remove(h, nn);
        if removed.ok { state_del(svc + ".transition." + h); }
    });
    state_set(service + ".transition." + host, to_json(#{old_target: old_target, new_name: new_name, new_port: new_host_port, image: image}));
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
    //
    // GUARDED on `proxy_switched` / `proxy_restored` (robustness review R6): the
    // engine's unwind is best-effort and error-isolated — if the restore-proxy
    // compensation below THROWS (an SSH blip, the proxy's health gate rejecting a
    // degraded old container, a bogus `old_target` per R10/R4b), the unwind still
    // continues to this one regardless. Removing the new container in that case
    // would blackhole traffic ONLY IF the proxy had actually been switched to it
    // (e) — if the health check below throws first (the most common failure mode),
    // the proxy was never switched away from the still-running old container, so
    // removing this never-healthy new container is safe (and correct — matches the
    // original, pre-R6-fix guarantee). Only skip the removal when the proxy was
    // switched AND the restore-back genuinely failed.


    // (c) Wait until the NEW container answers HTTP health on its new port, BEFORE
    //     switching traffic. (HTTP only — matching the legacy deploy; we do NOT
    //     require a Docker HEALTHCHECK, which many images don't define.)
    //
    // Robustness review R7-health: this used to build "http://" + host + ":" + new_host_port +
    // health_path and GET it from the CONTROL machine — but `host` is the raw SSH host string
    // (e.g. "deploy@web1" per docs/deploy.md), and userinfo isn't valid in an HTTP authority; even
    // a plain hostname is commonly firewalled to just 22/tcp from the control network, with the
    // app's ephemeral host port never exposed to anything but the host itself. Either way, a
    // perfectly healthy container failed this check and unwound an otherwise-successful fleet.
    // wait_healthy_on_host runs the SAME check from ON the host, over the ssh connection every
    // other operation here already uses — only SSH connectivity is required.
    print("    waiting for health...");
    health::wait_healthy_on_host(host, new_host_port, #{
        path: health_path,
        attempts: health_attempts,
        interval: health_interval,
        consecutive: health_consecutive,
        timeout: health_timeout,
    });

    // (d) Register the restore-proxy compensation BEFORE switching. It runs FIRST
    //     in the LIFO unwind, so on rollback traffic flows back to the still-running
    //     old container BEFORE the new one is removed (no blackhole window). Sets
    //     `proxy_restored` only once `px_deploy` returns WITHOUT throwing, so the
    //     rm-new compensation above can tell whether removing the new container is
    //     actually safe (R6).
    on_rollback(|| {
        px_deploy(pk, h, svc, ot, pcfg);
        proxy_restored = true;
    });

    // (e) Switch proxy traffic to the NEW container.
    let new_target = "localhost:" + new_host_port;
    print("    switching traffic -> " + new_target);
    proxy_switched = true; // Dispatch may commit even if its acknowledgment is lost.
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
// older deploy), we fall back to defaults and warn. Takes the SAME cross-machine
// deploy lock (R15) as deploy() itself, but for the WHOLE rollback (the `.prev`
// snapshot AND the nested deploy() call) — not just deploy()'s own inner portion
// — so a rollback refused by lock contention never clobbers its own rollback
// target first. `cfg.skip_lock: true` opts out, same as deploy().
fn rollback(hosts, service, cfg) {
    // Same R29 guard as deploy() (which this function calls internally), checked HERE TOO and
    // first: rollback() mutates `<service>.prev` (below, before calling deploy()) as a real,
    // persisted side effect. If rollback() only relied on deploy()'s own guard, a nested call
    // would advance `.prev` to the CURRENT image before deploy() throws — so a caller who reads
    // the error and retries rollback() at the top level would then roll back to the current
    // image instead of the one they intended. Checking here first keeps the refusal side-effect
    // free.
    if in_transaction() {
        throw "rollback() cannot be called from inside an active transaction() — nesting it " +
              "would let an unrelated later failure resurrect this rollback's already-committed " +
              "rollback compensations against state that no longer matches them, causing a real " +
              "outage (robustness review R29). Call rollback() at the top level for each service " +
              "instead.";
    }

    // Same R21 guard as deploy() (which this function calls internally), and checked here first
    // for the SAME reason as the R29 guard above: rollback() mutates `<service>.prev` (below,
    // before calling deploy()) as a real, persisted side effect. If rollback() only relied on
    // deploy()'s own guard, an empty-hosts call would still advance `.prev` before deploy()
    // throws — a caller who reads the error and retries with real hosts would then roll back
    // from the wrong starting point.
    if hosts.is_empty() {
        throw "rollback() called with an empty hosts array for '" + service + "' — nothing " +
              "would actually be rolled back (robustness review R21). Pass at least one host.";
    }

    // Same reasoning as the R29/R21 guards above, for the SAME `.prev`-mutation-before-throw
    // hazard (found during this fix's own final review): a caller-supplied `cfg.keep_images`
    // override reaches deploy()'s own validation (via the `cfg.keys()` merge below), but only
    // AFTER `.prev` has already been overwritten with the CURRENT (possibly broken) image. A
    // caller who passed an invalid negative override, hit the throw, fixed it, and retried
    // `rollback(hosts, service)` with no override would then "roll back" to the image they were
    // trying to escape — the real target lost. Checked here first, mirroring deploy()'s own
    // validation exactly, so the refusal stays side-effect free.
    if cfg.contains("keep_images") && cfg.keep_images < 0 {
        throw "rollback() called with a negative cfg.keep_images (" + cfg.keep_images + ") for '" +
              service + "' — must be >= 0 (0 keeps only the current and, if any, previous " +
              "version; omit the key entirely to disable tagged-image pruning) (robustness " +
              "review R22).";
    }

    let image = if cfg.contains("image") { cfg.image } else { "" };
    if image == "" {
        if has_state(service + ".prev") {
            image = state_get(service + ".prev");
            // Robustness review R10: a snapshotted ".prev" that's itself a MUTABLE tag can't be
            // safely rolled back to — the registry may have already moved that tag on to the very
            // broken build we're trying to escape, so "rolling back" to it would silently redeploy
            // the SAME broken image instead of a working one. An explicit cfg.image override (the
            // `if image == ""` branch above) is a deliberate caller choice and is NOT second-guessed
            // here — only the automatic, caller-invisible ".prev" snapshot is refused.
            // Case-insensitive: Docker's tag charset allows uppercase, so "LATEST"/"Latest" are
            // syntactically distinct, valid tags carrying the exact same mutable-pointer risk.
            if extract_version(image).to_lower() == "latest" {
                throw "Refusing to roll back " + service + " to \"" + image + "\": \":latest\" is a " +
                      "mutable tag, so the registry may already have been overwritten by the very " +
                      "broken build you're trying to escape — a \"rollback\" to it could silently " +
                      "redeploy the SAME broken image instead of a working one (robustness review " +
                      "R10). Specify an immutable image explicitly instead, e.g. " +
                      "rollback(hosts, \"" + service + "\", #{ image: \"repo:v1.2.3\" }) or a digest " +
                      "reference (\"repo@sha256:...\").";
            }
        } else {
            let hint = "";
            if has_state(service + ".image") && extract_version(state_get(service + ".image")).to_lower() == "latest" {
                hint = " (this service is on a mutable \":latest\" tag, so repeat deploys under the " +
                       "same tag never record a distinct rollback snapshot — robustness review R10)";
            }
            throw "No rollback image found for " + service + hint + ". Specify one explicitly (cfg.image).";
        }
    }

    print("==> Rolling back " + service + " to " + image);

    // Start from the persisted effective config so the rollback replays the real deploy shape.
    let replay = #{};
    if has_state(service + ".config") {
        let config_key = if has_state(service + ".prev") && image == state_get(service + ".prev") && has_state(service + ".prev_config") { ".prev_config" } else { ".config" };
        replay = from_json(state_get(service + config_key));
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

    // Same reasoning as the R29/R21/keep_images guards above, for the SAME
    // `.prev`-mutation-before-throw hazard (found during this fix's own final review): deploy()
    // itself refuses a `domain` set on the kamal-proxy backend ("kamal-proxy silently ignored
    // cfg.domain"), but only reached via the `deploy(hosts, image, service, replay)` call below —
    // AFTER `.prev` has already been overwritten. A caller who passed `cfg: #{ proxy: "kamal" }`
    // to override a persisted `domain: "..."` deploy, hit the throw, fixed it, and retried
    // `rollback(hosts, service)` with no override would then "roll back" to the image they were
    // trying to escape. Checked here first, mirroring deploy()'s own validation exactly.
    let replay_proxy = if replay.contains("proxy") { replay.proxy } else { "kamal" };
    let replay_domain = if replay.contains("domain") { replay.domain } else { "" };
    if replay_proxy != "caddy" && replay_domain != "" {
        throw "rollback() would replay cfg.domain (" + replay_domain + ") against the kamal-proxy " +
              "backend for '" + service + "' — kamal-proxy does not support domain-based routing " +
              "or automatic TLS in this codebase. Pass cfg.proxy: \"caddy\" (or omit the domain " +
              "override) to rollback().";
    }

    // Fable final review of the 2.6 lifecycle-hooks slice: `hook_pre_deploy` fires from the
    // nested `deploy()` call below (it's `deploy()`'s own first statement), but `deploy()`'s
    // try/catch can't help HERE — this function overwrites `.prev` further down, BEFORE calling
    // deploy(), so a hook that BLOCKS the deploy corrupts the rollback chain: `.prev` is already
    // clobbered with the current (broken) image by the time the hook throws, so a caller who
    // retries `rollback(hosts, service)` after the block clears would silently "roll back" to the
    // very image they were trying to escape, reporting success. Same class of hazard as the
    // R29/R21/keep_images/domain guards above — call the hook HERE, before ANY mutation, so a
    // block stays side-effect free. `replay.__skip_pre_deploy_hook` (set below, alongside
    // `replay.skip_lock`) suppresses the nested deploy()'s own call so the hook doesn't fire
    // twice per rollback.
    call_pre_deploy_hook(service, image, hosts);

    // Deferred robustness finding (found reviewing the `nrg rollback` CLI, round 5): rollback()
    // must take the SAME cross-machine lock deploy() takes (R15) HERE — before the `.prev`
    // mutation below — not just rely on deploy()'s own acquire, which only runs AFTER `.prev` has
    // already been overwritten. Without this, a rollback refused by lock contention (another
    // deploy/rollback of this service already in progress) still destroys the very rollback
    // target it was trying to preserve. Mirrors the R29/R21/keep_images/domain guards above:
    // checked (here, acquired) before any mutation, so a refusal stays side-effect free.
    let skip_lock = if cfg.contains("skip_lock") { cfg.skip_lock } else { false };
    let lock_hosts = hosts;
    lock_hosts.sort();
    lock_hosts.dedup();
    let lock_dir = "/tmp/nrg-deploy-lock-" + service;
    if !skip_lock {
        for lock_host in lock_hosts { acquire_deploy_lock(lock_host, lock_dir, service); }
    }

    // Fable's final review of this fix: the `.prev` mutation below is itself fallible
    // (`state_set` returns a Result the builtin throws on — e.g. a disk-full/I/O failure
    // persisting state), so it must run INSIDE the same try/catch as the nested deploy() call,
    // not before it — otherwise a failure here would leak the lock with no release, the exact
    // "any failure after acquire releases the lock" property this fix exists to guarantee.
    try {
        // rollback() now holds the lock itself (above) for its whole duration (the `.prev`
        // mutation AND the nested deploy() call) — `replay.skip_lock` is forced `true` so the
        // nested deploy() doesn't try to re-acquire a lock rollback() already holds, which would
        // otherwise throw "already locked" against itself every time.
        replay.skip_lock = true;
        // rollback() already called call_pre_deploy_hook itself, above, before the `.prev`
        // mutation (Fable final review) — suppress the nested deploy()'s own call so the hook
        // doesn't fire a second time for the same rollback.
        replay.__skip_pre_deploy_hook = true;
        deploy(hosts, image, service, replay);
    } catch (err) {
        if !skip_lock { for lock_host in lock_hosts { release_deploy_lock(lock_host, lock_dir, service); } }
        throw err;
    }
    if !skip_lock { for lock_host in lock_hosts { release_deploy_lock(lock_host, lock_dir, service); } }

    // Roadmap 2.6: an optional, orchestration-file-defined `hook_post_rollback(service, image,
    // hosts)` — called AFTER the lock is released, in ADDITION to (not instead of) the nested
    // deploy() call's own `hook_post_deploy` above, since a rollback IS a deploy of a different
    // image — this lets a caller's hooks tell the two apart (e.g. `hook_post_deploy` posts a
    // routine "vNN live" message on every deploy, while `hook_post_rollback` pages someone,
    // since a rollback usually means something just went wrong). Best-effort, same convention as
    // `hook_post_deploy`.
    call_post_rollback_hook(service, image, hosts);
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

    // Robustness review R10b: a STOPPED-but-present container by this name (a prior crashed run,
    // or a manual `docker stop`) used to make `docker run --name` below fail with "the container
    // name ... is already in use" — correctly thrown as an error (see the `!r.ok` check below),
    // but that meant every future deploy stayed permanently wedged until an operator manually
    // removed it by hand. Idempotent teardown first (same `rm -f ... || true` shape every other
    // start-fresh path in this codebase already uses) makes a stopped-but-present accessory
    // self-heal on the very next deploy instead.
    docker::docker_remove(host, name);

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

    // Robustness review R10b: `docker run -d`'s exit code only reflects that the container
    // STARTED — it says nothing about whether it's still running a moment later. A misconfigured
    // accessory (e.g. a database given the wrong credentials) can exit almost immediately after
    // starting, and `r.ok` alone can't tell that apart from a genuinely healthy start — the app
    // would then deploy against a dead accessory with no signal anything went wrong. This is
    // deliberately NOT a full health check (accessories have no `health_path` concept in this
    // stdlib) — just a brief re-probe to catch the "exited almost immediately" case.
    sleep(1);
    if !docker::docker_container_running(host, name) {
        throw "Accessory " + name + " on " + host + " exited immediately after starting — check " +
              "its logs (`docker logs " + name + "` on " + host + ", e.g. via `nrg ssh " + host +
              "`) for why (robustness review R10b).";
    }
}

// accessory_run(host, name, image) — 3-arg overload (defaults).
fn accessory_run(host, name, image) {
    accessory_run(host, name, image, #{})
}

// accessory_stop(host, name) — stop a long-lived accessory container, keeping it (and its named
// volumes) around for a later accessory_restart/accessory_upgrade. Idempotent: a no-op, not an
// error, if the accessory isn't running (matches docker_stop's own `|| true` semantics one level
// up, so a caller never has to check first).
fn accessory_stop(host, name) {
    if !docker::docker_container_running(host, name) {
        print("  accessory " + name + " already stopped on " + host);
        return;
    }
    print("  stopping accessory " + name + " on " + host);
    docker::docker_stop(host, name);
}

// accessory_restart(host, name) — restart the EXISTING accessory container in place (`docker
// restart`), reusing whatever image/config it's already running — NOT a re-create, and no image
// argument because none is needed. Useful to pick up a config change delivered via a mounted
// volume, or clear a stuck process, without touching the image itself. For an image bump, use
// accessory_upgrade instead — restarting the same container can never change what image it runs.
// Goes through docker::docker_restart (sim-routed), not a hand-built ssh_exec, so a dry-run
// restart marks the container running again in the sim overlay — otherwise a later read in the
// SAME script (e.g. a subsequent accessory_run) would see the pre-restart reality instead of what
// a live run would actually produce (Fable final review).
fn accessory_restart(host, name) {
    print("  restarting accessory " + name + " on " + host);
    let r = docker::docker_restart(host, name);
    if !r.ok {
        throw "Failed to restart accessory " + name + " on " + host + ":\n" + r.stderr;
    }
}

// accessory_upgrade(host, name, image, cfg) / accessory_upgrade(host, name, image) — bump an
// existing accessory to a new image, keeping its named volume(s). This is the route
// accessory_run itself can never provide: accessory_run's "already running" check is BY NAME
// only, so a running "myapp-db" container permanently blocks it from ever noticing an image
// bump — the first `postgres:16` -> `postgres:17` this stdlib had no answer for. Pulls the NEW
// image FIRST (Opus review: matching deploy()'s own pull-before-transaction ordering) so a bad
// tag or registry-auth failure throws with the OLD container still up, instead of destroying it
// first and only then discovering the new image can't run. Only after that succeeds does it stop
// and remove the OLD container (never `-v`, so named volumes are untouched — a bind-mounted host
// path is unaffected either way, since removing a container never touches the host filesystem),
// then start `name` fresh on the NEW image via accessory_run itself, reusing its own
// start-and-verify logic so a misconfigured new image that exits immediately is caught exactly
// the way a fresh accessory_run would catch it.
//   cfg: #{ ports: #{}, envs: #{}, volumes: #{}, network: "", cmd: "" }  — same shape as
//   accessory_run's; pass the SAME cfg you deployed the old image with, unless the upgrade is
//   also changing ports/envs/etc.
fn accessory_upgrade(host, name, image, cfg) {
    print("  upgrading accessory " + name + " to " + image + " on " + host);
    docker::docker_pull(host, image);
    docker::docker_stop(host, name);
    docker::docker_remove(host, name);
    accessory_run(host, name, image, cfg);
}

// accessory_upgrade(host, name, image) — 3-arg overload (defaults).
fn accessory_upgrade(host, name, image) {
    accessory_upgrade(host, name, image, #{})
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

// extract_version(image) — the image TAG: the part after the last `:` that comes AFTER the last
// `/`, else "latest". Splitting on the last `:` of the WHOLE ref is wrong for a registry-with-port
// reference like `registry.example.com:5000/app` — that `:5000` is the registry port, not a tag,
// and would yield the invalid container-name fragment `5000/app` (issue #28). So we look at the
// last path segment only.
private fn extract_version(image) {
    let segs = image.split("/");
    let last = segs[segs.len() - 1];      // e.g. "app:v2", "app", or "5000" is impossible here
    if last.contains(":") {
        let tagparts = last.split(":");
        tagparts[tagparts.len() - 1]
    } else {
        "latest"
    }
}

// extract_repo(image) — the bare repo (everything except the trailing `:<tag>` identified the
// same way extract_version identifies it), used by R22's keep_images pruning to ask "what other
// tags of THIS SAME repo exist on the host?" A Docker/Podman image name's last path segment can
// contain at most one `:` (the tag separator) — the registry-host:port ambiguity extract_version
// documents is already resolved by only ever splitting the LAST `/`-segment.
private fn extract_repo(image) {
    let segs = image.split("/");
    let last = segs[segs.len() - 1];
    if last.contains(":") {
        let name = last.split(":")[0];
        let prefix = segs.extract(0..segs.len() - 1);
        if prefix.is_empty() { name } else { join(prefix, "/") + "/" + name }
    } else {
        image // no tag at all — the whole string is already the repo
    }
}

// lock_host_for(hosts) -> string — the host the R15 cross-machine deploy lock is anchored on:
// the ALPHABETICALLY-FIRST host, not `hosts[0]`. Two operators deploying the exact same fleet
// with a differently-ordered host array (e.g. `["web2","web1"]` vs `["web1","web2"]`) must
// still contend for the SAME lock, or the whole mutual-exclusion guarantee silently evaporates
// (Fable review, full-project pass). Sorts a COPY — `hosts.sort()` would mutate the caller's
// own array in place, reordering the actual rolling-deploy sequence (deploy() iterates `hosts`
// in the caller's given order deliberately; only the LOCK's host choice should be order-
// independent). `nrg lock` (native Rust) already documents this exact "alphabetically sorted,
// not deploy order" convention for its own auto-detect — see StateStore::hosts_for.
private fn lock_host_for(hosts) {
    let sorted = hosts;
    sorted.sort();
    sorted[0]
}

// acquire_deploy_lock(lock_host, lock_dir, service) — throws if `lock_dir` already exists on
// `lock_host` (another deploy/rollback of this service is in progress, possibly from a different
// machine), or if the `mkdir` itself couldn't even run (SSH-level failure, unrelated to
// contention). `mkdir` is the atomic exclusive-create primitive here — no separate compare-and-
// swap needed, since a plain directory create either succeeds (lock acquired) or fails with
// "File exists" (already held), and those two outcomes are distinguished by stderr content the
// same way this codebase's existing probe classifiers (R4/R31/R32) distinguish failure kinds.
// The `holder` file written after is best-effort diagnostics ONLY — its own failure does not
// undo the already-acquired lock, so it's never checked.
private fn acquire_deploy_lock(lock_host, lock_dir, service) {
    remote_lock_acquire(lock_host, lock_dir);
}
private fn release_deploy_lock(lock_host, lock_dir, service) {
    remote_lock_release(lock_host, lock_dir);
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

// normalize_arch(arch) — map a raw `uname -m` string to a Docker platform arch name, so the
// SAME architecture reported differently by different `uname`s isn't mistaken for a mismatch —
// notably macOS and Linux both say "arm64" OR "aarch64" for 64-bit ARM depending on the OS, and
// comparing the raw strings would falsely flag an ARM laptop deploying to an ARM VPS.
private fn normalize_arch(arch) {
    if arch == "x86_64" {
        "amd64"
    } else if arch == "aarch64" || arch == "arm64" {
        "arm64"
    } else {
        arch
    }
}

// check_arch_mismatch(build_host, host) — compare the BUILD machine's CPU architecture to
// `host`'s, and THROW with a clear, actionable message if they differ, before any build/push/pull
// runs. Without this, a mismatch surfaces only when the container fails to start on the host with
// an opaque exec-format error — well after the deploy looked like it was succeeding.
//
// `build_host` (roadmap 1.1 step 3a) is the machine the build will actually run ON — "" (the
// default) means this machine (docker_build's own local path). Comparing against the WRONG
// machine here would be worse than not checking at all: it could throw a false mismatch when
// `build_host` is set specifically because it DOES match `host`'s architecture (defeating the
// whole point of setting it), or silently pass a real mismatch when `build_host` differs from
// this machine's own architecture.
//
// DRY-RUN CAVEAT: local_exec is a MUTATING-class builtin, so under --dry-run it's stubbed and
// can't read this machine's REAL architecture (mirrors rt::auto_detect()'s own documented
// caveat). Comparing a real remote probe against a fake local value would be worse than not
// checking at all, so the check is skipped entirely under --dry-run — this is a LIVE-run-only
// safeguard (consistent with the rest of the live deploy path's test-coverage limits; see
// docs/robustness-review.md R8).
private fn check_arch_mismatch(build_host, host) {
    if is_dry_run() {
        print("  [note] skipping build-arch check under --dry-run (local probe is stubbed)");
        return;
    }

    let build_label;
    let build_raw;
    if build_host == "" {
        build_label = "this machine";
        let local = local_exec("uname -m");
        build_raw = local.stdout;
    } else {
        build_label = build_host;
        // ssh_probe (unlike ssh_exec) is read-only and honest under dry-run too, but we already
        // returned above in that case — this only ever runs live.
        let probe = ssh_probe(build_host, "uname -m");
        if !probe.ok {
            return; // build_host unreachable (e.g. a fresh box) — a different failure, not ours to report
        }
        build_raw = probe.stdout;
    }
    build_raw.trim();
    if build_raw == "" {
        return; // couldn't determine the build machine's arch; don't block the deploy on it
    }

    let remote = ssh_probe(host, "uname -m");
    if !remote.ok {
        return; // host unreachable (e.g. a fresh box) — that's a different failure, not ours to report
    }
    let remote_raw = remote.stdout;
    remote_raw.trim();
    if remote_raw == "" {
        return;
    }

    let build_arch = normalize_arch(build_raw);
    let remote_arch = normalize_arch(remote_raw);
    if build_arch != remote_arch {
        // `normalize_arch` only maps the two Docker platform arch NAMES we're confident about
        // (amd64/arm64); anything else (armv7l, ppc64le, ...) is an identity pass-through, and
        // "linux/<that>" isn't necessarily a valid buildx platform string (32-bit ARM's real
        // form is "linux/arm/v7", not "linux/armv7l") — so only suggest a concrete cfg.platform
        // value when we actually know it's right.
        let suggestion = if remote_arch == "amd64" || remote_arch == "arm64" {
            "cfg.platform: \"linux/" + remote_arch + "\""
        } else {
            "the appropriate cfg.platform value for " + remote_arch
        };
        let build_here = if build_host == "" { "here" } else { "on " + build_host };
        throw "Build-arch mismatch: " + build_label + " is " + build_arch + " (" + build_raw +
              ") but " + host + " is " + remote_arch + " (" + remote_raw + "). Building " +
              build_here + " would produce an image that fails to start on " + host +
              " with an exec-format error. Pass " + suggestion + " to deploy(), or set " +
              "cfg.build_host to a machine matching " + host + "'s architecture.";
    }
}
