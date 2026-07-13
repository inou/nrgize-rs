// bunny.rhai — Bunny Magic Containers deploy target (roadmap 2.9, Phase 2).
//
// A second, PARALLEL deploy target alongside the SSH+Docker path (lib/deploy.rhai) — Bunny Magic
// Containers is a managed container PaaS with no SSH surface at all, driven entirely through a
// REST API. This module is a thin Rhai wrapper over the already-shipped `http_get`/`http_patch`
// builtins (Phase 1, roadmap 2.9) — zero new Rust engine code, matching this codebase's own
// "ssh_exec is a primitive, deploy() is stdlib built on it" layering.
//
// Usage:
//   import "lib/bunny" as bunny;
//   bunny::deploy_app(#{
//       app_id: "12345", api_key: secret("BUNNY_API_KEY"),
//       container: "web", image_tag: "v42",
//   });
//   bunny::wait_for_image(#{
//       app_id: "12345", api_key: secret("BUNNY_API_KEY"),
//       container: "web", image_tag: "v42",
//   });
//   // ...and if it goes wrong:
//   bunny::rollback_app(#{app_id: "12345", api_key: secret("BUNNY_API_KEY"), container: "web"});
//
// GROUND TRUTH (verified 2026-07-13 against Bunny's own public, official GitHub Action source,
// `BunnyWay/actions/container-update-image/src/action.ts` — the action Bunny's own docs tell users
// to wire into CI for automated image updates):
//
//   GET https://api.bunny.net/mc/apps/{appId}
//     headers: Accept: application/json, AccessKey: <api_key>
//     -> 200: { id, containerTemplates: [ { id, name, ... }, ... ] }
//     -> 400: bad app_id; anything else non-200: generic HTTP failure
//
//   PATCH https://api.bunny.net/mc/apps/{appId}/containers/{containerId}
//     headers: Content-Type: application/json, AccessKey: <api_key>
//     body: { id: containerId, imageTag, imageName?, imageDigest? }
//     -> 200: accepted; anything else: failure
//
// Auth is a single `AccessKey: <key>` header — NOT `Authorization: Bearer`. A container is
// identified by NAME in this module's `cfg` (matching the real action's own `container` input),
// but the API itself addresses it by ID: the real action GETs the app, filters
// `containerTemplates` by `.name == containerName`, throws if it finds zero or more than one
// match, then PATCHes using the matched `.id`. `find_container` below reproduces that exact
// lookup and both exact failure shapes — the real, already-shipped behavior of Bunny's own
// tooling, not invented wording for the same failure.
//
// ONE STRUCTURAL INFERENCE, FLAGGED, NOT INDEPENDENTLY VERIFIED: the fetched source's
// `containerTemplates[i]` TypeScript type only *declares* `id`/`name` (the two fields that one
// action happens to use) — it doesn't rule out (or confirm) an `imageTag` field alongside them.
// Given the PATCH body is literally `{id, imageTag, imageName?, imageDigest?}` — exactly what one
// `containerTemplates` entry needs to describe — `container_image_tag` below ASSUMES
// `containerTemplates[i].imageTag` reflects the currently-deployed tag. Every doc/support page
// this research pass tried to fetch for independent confirmation returned HTTP 403. If your Bunny
// account's real GET response uses a different field name, fix it in `container_image_tag` (one
// line) — every reader (`current_image_tag`, `deploy_app`'s snapshot step) goes through it, so
// nothing else needs to change.
//
// DRY-RUN: no explicit branching anywhere below. GET is already an honest, real read of existing
// reality even under `--dry-run` (see docs/builtins.md's `http_get` semantics), and PATCH already
// short-circuits under `--dry-run` to a synthetic 200 + a recorded `check` action — both inherited
// for free from Phase 1, exactly as the roadmap's Phase 2 line promises ("no new Rust needed").
// The one exception is `wait_for_image`'s polling loop, which has no live state to observe under
// dry-run (the PATCH never really ran) — it short-circuits via the global `is_dry_run()` builtin,
// the same escape hatch `lib/healthcheck.rhai::ssh_http_status` already established for the
// identical "can't observe a not-really-changed remote thing under dry-run" problem.

// Only deploy_fleet (Phase 3) uses this — a canary/batch target's optional health_url is checked
// through the SAME wait_healthy/sim_http_healthy path every other health-gated cutover in this
// codebase uses, not a Bunny-specific reimplementation.
import "lib/healthcheck" as health;

// api_key may be a bare secret(...) or a plain string — mirrors lib/notify.rhai::webhook's exact
// Secret-or-plain-string handling for its own credential-shaped argument.
private fn reveal_key(api_key) {
    if type_of(api_key) == "Secret" { reveal(api_key) } else { api_key }
}

private fn auth_headers(api_key) {
    #{"AccessKey": reveal_key(api_key), "Accept": "application/json"}
}

// cfg.base_url overrides the real Bunny API host — undocumented-by-default (every example in this
// file omits it), but not a test-only hack: it's the same "cfg key with a sensible default"
// pattern this stdlib already uses everywhere else, and it's what lets tests/bunny.rs point this
// module at a real local listener instead of the actual internet, per this codebase's own
// "exercise the real HTTP round trip, don't mock ureq" convention.
private fn base_url(cfg) {
    if cfg.contains("base_url") && cfg.base_url != "" { cfg.base_url } else { "https://api.bunny.net" }
}

// app_config(cfg) -> map — GET the app's full config. cfg: #{app_id, api_key}.
fn app_config(cfg) {
    let url = base_url(cfg) + "/mc/apps/" + cfg.app_id;
    let r = http_get(url, auth_headers(cfg.api_key));
    if r.status == 400 {
        throw "bunny::app_config: could not obtain app configuration — double-check " +
              "cfg.app_id (\"" + cfg.app_id + "\"): Bunny returned HTTP 400.";
    }
    if r.status != 200 {
        throw "bunny::app_config: could not obtain app configuration: HTTP status " +
              r.status + ".";
    }
    from_json(r.body)
}

// find_container(app_config, container_name) -> map — locate a container by name. Throws if zero
// or more than one container matches (mirroring Bunny's own GitHub Action's two distinct failure
// cases for the same lookup), or if the response doesn't even have the expected shape (a raw
// Rhai "cannot iterate" error would otherwise be the only signal of an unexpected app config).
fn find_container(app_config, container_name) {
    if !app_config.contains("containerTemplates") {
        throw "bunny::find_container: app \"" + app_config.id + "\"'s configuration has no " +
              "containerTemplates — unexpected response shape from Bunny's API.";
    }
    let matches = [];
    for c in app_config.containerTemplates {
        if c.name == container_name {
            matches.push(c);
        }
    }
    if matches.len() == 0 {
        throw "bunny::find_container: could not find a container named \"" + container_name +
              "\" in app \"" + app_config.id + "\".";
    }
    if matches.len() > 1 {
        throw "bunny::find_container: found more than one container named \"" + container_name +
              "\" in app \"" + app_config.id + "\".";
    }
    matches[0]
}

// container_image_tag(container) -> string | () — THE single place this file reads a container's
// currently-deployed tag off a GET response. Carries this file's flagged imageTag-field
// inference — see the header comment above. Every caller (current_image_tag, deploy_app's
// snapshot step) MUST go through this function, not `container.imageTag` directly, so a future
// fix to the field name only ever needs to change one line.
private fn container_image_tag(container) {
    container.imageTag
}

// current_image_tag(cfg) -> string — convenience read. cfg: #{app_id, api_key, container}.
fn current_image_tag(cfg) {
    let container = find_container(app_config(cfg), cfg.container);
    container_image_tag(container)
}

// The shared PATCH path both deploy_app and rollback_app drive, given an ALREADY-RESOLVED
// container id (the caller has already done its own single app_config()+find_container() lookup
// — this function makes no GET of its own, so a full deploy_app/rollback_app call is exactly one
// GET + one PATCH, matching the real Bunny GitHub Action's own request shape exactly).
//
// image_name/image_digest are explicit PARAMETERS, not read off `cfg` directly (Fable final
// review): `deploy_app` and `rollback_app` share one `cfg` shape in the natural calling pattern
// (build one cfg map, deploy_app(cfg) then later rollback_app(cfg)) — reading these optional
// fields off `cfg` inside this shared function would have let a rollback silently carry the
// NEW deploy's image_name/image_digest alongside the OLD (rolled-back-to) tag. Since a digest
// pin normally takes precedence over a tag, that would make "rollback" silently keep the bad
// image while still reporting success — a silent non-rollback in a deployment-safety tool.
// Passing "" explicitly (rollback_app never has these to give) guarantees rollback's PATCH body
// is always exactly {id, imageTag}, never contaminated by a caller's reused cfg map.
// Builds the {url, body} pair for a container PATCH — shared by patch_container (single,
// sequential) and deploy_batch (fanned out via http_patch_all), Opus review: this used to be
// copy-pasted between the two, so a future change to the PATCH shape in one wouldn't have
// reached the other even though they were byte-identical today.
private fn build_patch_request(cfg, container_id, image_tag, image_name, image_digest) {
    let body = #{id: container_id, imageTag: image_tag};
    if image_name != "" {
        body.imageName = image_name;
    }
    if image_digest != "" {
        body.imageDigest = image_digest;
    }
    #{
        url: base_url(cfg) + "/mc/apps/" + cfg.app_id + "/containers/" + container_id,
        body: to_json(body),
    }
}

private fn patch_container(cfg, container_id, image_tag, image_name, image_digest) {
    let req = build_patch_request(cfg, container_id, image_tag, image_name, image_digest);
    let r = http_patch(req.url, req.body, auth_headers(cfg.api_key));
    if r.status != 200 {
        throw "bunny::patch_container: could not save container configuration: HTTP status " +
              r.status + "." + patch_failure_detail(r);
    }
    r
}

// Fable final review: status 0 means a transport failure (DNS/TLS/timeout/connection refused —
// see http_patch's own doc), and r.body carries the actual cause in that case ("request failed:
// ..."). Surfacing it turns a useless "HTTP status 0." into something an operator can act on —
// especially in deploy_fleet's fan-out, where a transport failure is the likeliest per-target
// failure mode and the abort summary otherwise names a target with no usable reason at all.
private fn patch_failure_detail(r) {
    if r.status == 0 { " (" + r.body + ")" } else { "" }
}

private fn prev_key(app_id, container) {
    "bunny." + app_id + "." + container + ".prev"
}

// Snapshots `container`'s CURRENT image tag into state (bunny.<app_id>.<container>.prev), or
// prints the documented skip notice if the container reports none. Shared by deploy_app and
// deploy_fleet's batch phase (Phase 3) so the snapshot rule lives in exactly one place.
private fn snapshot_current_tag(app_id, container_name, container) {
    let current = container_image_tag(container);
    if current != () {
        state_set(prev_key(app_id, container_name), current);
    } else {
        print("  bunny: no current imageTag reported for \"" + container_name +
              "\" — skipping the rollback snapshot (nothing to revert to yet).");
    }
}

// deploy_app(cfg) -> HttpResponse — upgrade one Bunny app's container to a new image.
// cfg: #{app_id, api_key, container, image_tag, image_name?, image_digest?}.
// Snapshots the CURRENT image tag into state (bunny.<app_id>.<container>.prev) before patching,
// so rollback_app(cfg) can revert it later — mirroring lib/deploy.rhai's own single-snapshot
// `<service>.prev` convention (see docs/roadmap.md's documented "rollback only ever targets the
// single snapshotted .prev" limitation — this module inherits the same, deliberate scope).
fn deploy_app(cfg) {
    let container = find_container(app_config(cfg), cfg.container);
    snapshot_current_tag(cfg.app_id, cfg.container, container);
    let image_name = if cfg.contains("image_name") { cfg.image_name } else { "" };
    let image_digest = if cfg.contains("image_digest") { cfg.image_digest } else { "" };
    patch_container(cfg, container.id, cfg.image_tag, image_name, image_digest)
}

// rollback_app(cfg) -> HttpResponse — revert to the previously-deployed tag.
// cfg: #{app_id, api_key, container}. Throws if no previous image was ever snapshotted (i.e.
// deploy_app was never called for this app_id/container in this state store). Deliberately never
// forwards image_name/image_digest even if a caller's cfg happens to still carry them from a
// prior deploy_app(cfg) call using the SAME map — a rollback's PATCH body is always exactly
// {id, imageTag}, see patch_container's own comment above. `.prev` only ever snapshots the tag,
// so a digest-pinned deploy's rollback cannot restore the previous digest, only the previous tag.
fn rollback_app(cfg) {
    let prev = state_get(prev_key(cfg.app_id, cfg.container));
    if prev == () {
        throw "bunny::rollback_app: no previous image recorded for app \"" + cfg.app_id +
              "\" container \"" + cfg.container + "\" — nothing to roll back to.";
    }
    let container = find_container(app_config(cfg), cfg.container);
    patch_container(cfg, container.id, prev, "", "")
}

// wait_for_image(cfg) -> () — poll until the deploy has propagated.
// cfg: #{app_id, api_key, container, image_tag, attempts?: 30, interval?: 2}.
// Dry-run: short-circuits immediately (no polling, no real GET) — there is no live change to
// observe, since patch_container's own PATCH never really ran under --dry-run. Live: polls
// current_image_tag until it matches cfg.image_tag; throws after exhaustion.
fn wait_for_image(cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let interval = if cfg.contains("interval") { cfg.interval } else { 2 };

    if attempts < 1 {
        throw "bunny::wait_for_image: cfg.attempts must be >= 1 (got " + attempts + ").";
    }

    if is_dry_run() {
        print("  bunny::wait_for_image: dry-run — assuming \"" + cfg.image_tag +
              "\" has propagated to \"" + cfg.container + "\" (no real GET performed).");
        return;
    }

    let last = ();
    for i in 0..attempts {
        last = current_image_tag(cfg);
        if last == cfg.image_tag {
            print("  bunny: \"" + cfg.container + "\" now reports image \"" + last + "\" (" +
                  (i + 1) + "/" + attempts + ")");
            return;
        }
        if i < attempts - 1 {
            sleep(interval);
        }
    }

    throw "bunny::wait_for_image: \"" + cfg.container + "\" never reported image \"" +
          cfg.image_tag + "\" after " + attempts + " attempts (last seen: \"" + last + "\").";
}

// ---------------------------------------------------------------------------
// deploy_fleet — Phase 3: canary-then-batched-parallel rollout across many targets.
// ---------------------------------------------------------------------------
//
// Motivating case (see the design spec): a multi-tenant SaaS control plane pushing one image
// across potentially hundreds of per-tenant Bunny apps — deploy_app's strictly-sequential,
// one-GET-one-PATCH-then-poll shape does not scale to that time-wise, and a bad image should
// never reach the WHOLE fleet before anyone notices.
//
// cfg (shared across the whole fleet — one Bunny account):
//   #{
//       api_key,
//       canary_size?: 1,       // targets[0..canary_size] go first, SEQUENTIALLY, fully verified
//       batch_size?: 5,        // concurrent PATCHes per batch for the REST of the fleet
//       max_failures?: 0,      // abort once this many targets (canary included) have failed
//       health_attempts?: 30, health_interval?: 2,   // forwarded to wait_for_image / health checks
//   }
// targets: Array of #{app_id, container, image_tag, image_name?, image_digest?, health_url?}
//   health_url, if present, is checked via lib/healthcheck's wait_healthy AFTER wait_for_image
//   confirms the tag itself has propagated — an extra, optional real-traffic-endpoint check.

private fn fleet_health_attempts(cfg) {
    if cfg.contains("health_attempts") { cfg.health_attempts } else { 30 }
}
private fn fleet_health_interval(cfg) {
    if cfg.contains("health_interval") { cfg.health_interval } else { 2 }
}

// Build a per-target cfg by layering `target`'s identity fields over a COPY of the shared fleet
// `cfg` (api_key/base_url/etc. all carry through by value — Rhai maps copy on assignment, see
// this file's own commit history for the empirical confirmation this reasoning already relies
// on elsewhere). image_name/image_digest are only set if `target` itself has them — the shared
// fleet `cfg` never carries those (they're documented as per-target-only keys), so there is
// nothing here that could leak one target's optional fields into another's, unlike the
// deploy_app/rollback_app cfg-reuse footgun patch_container's own comment describes.
private fn target_cfg(target, cfg) {
    let c = cfg;
    c.app_id = target.app_id;
    c.container = target.container;
    c.image_tag = target.image_tag;
    // Opus review: image_name/image_digest are documented as per-target-only keys — the shared
    // fleet cfg should never carry them — but that's a documented contract, not an enforced one.
    // If a caller mistakenly puts image_digest on the SHARED cfg (a natural "all my targets share
    // a base image" slip), every target would silently inherit it, and since a digest pin
    // overrides a tag, each target's own image_tag would be silently ignored fleet-wide — exactly
    // the class of footgun Phase 2's review rounds fixed once for patch_container's own cfg-reuse
    // path. Unconditionally clearing before the target-specific check closes it here too.
    c.remove("image_name");
    c.remove("image_digest");
    if target.contains("image_name") { c.image_name = target.image_name; }
    if target.contains("image_digest") { c.image_digest = target.image_digest; }
    // base_url is normally fleet-wide (one Bunny account, one API host) and belongs on the
    // shared cfg — but a per-target override is harmless to allow and is exactly what makes
    // realistic multi-listener testing possible (tests/bunny.rs points each target at its own
    // local mock server).
    if target.contains("base_url") { c.base_url = target.base_url; }
    c
}

// Fable final review: deploy_fleet used to trust each target's shape completely — a missing or
// mistyped image_tag silently became Rhai's unit value, which build_patch_request then serialized
// as a real, live PATCH body of {"imageTag":null} (an unknown-behavior write against the actual
// Bunny API), and — worse — this was invisible under --dry-run, since the malformed PATCH itself
// short-circuits successfully either way. A fleet's targets are exactly the "one bad element among
// hundreds, likely machine-generated" case this codebase already guards elsewhere (ssh_exec_all
// rejects a malformed host element by index, exec.rs) — so reject up front, before any network
// call, naming the offending index and field.
private fn validate_target(i, target) {
    if type_of(target) != "map" {
        throw "bunny::deploy_fleet: targets[" + i + "] must be a map (got " + type_of(target) + ").";
    }
    for key in ["app_id", "container", "image_tag"] {
        if !target.contains(key) || type_of(target[key]) != "string" || target[key] == "" {
            throw "bunny::deploy_fleet: targets[" + i + "] is missing a non-empty string \"" +
                  key + "\".";
        }
    }
}

private fn fleet_failure_summary(failed) {
    let parts = [];
    for f in failed {
        parts.push(f.app_id + " (" + f.error + ")");
    }
    join(parts, ", ")
}

// Canary path: deploy_app + wait_for_image + an optional health_url check, ONE target, never
// throws itself (a canary failure is data the caller aggregates, not an immediate abort by
// itself — deploy_fleet decides abort-or-continue against cfg.max_failures).
// Returns #{ok: bool, error: String} (error is "" when ok).
private fn deploy_one_target_verified(target, cfg) {
    let tc = target_cfg(target, cfg);
    let ok = true;
    let error = "";
    try {
        deploy_app(tc);
        let wait_cfg = tc;
        wait_cfg.attempts = fleet_health_attempts(cfg);
        wait_cfg.interval = fleet_health_interval(cfg);
        wait_for_image(wait_cfg);
        if target.contains("health_url") && target.health_url != "" {
            health::wait_healthy(target.health_url, #{
                attempts: fleet_health_attempts(cfg), interval: fleet_health_interval(cfg),
            });
        }
    } catch (err) {
        ok = false;
        error = "" + err;
    }
    #{ok: ok, error: error}
}

// Batch path: for `batch` (already sized to cfg.batch_size by the caller), do the per-target
// GET+find+snapshot SEQUENTIALLY (cheap reads — see this function's own doc in the Phase 3 plan),
// then fan the PATCH step out CONCURRENTLY via http_patch_all, then verify each target
// (wait_for_image + optional health_url) sequentially. A target that fails at ANY step (GET,
// PATCH, or verify) is reported in `failed`, never thrown — same aggregate-then-decide contract
// as the canary path. Returns #{succeeded: Array<String>, failed: Array<#{app_id, error}>}.
private fn deploy_batch(batch, cfg) {
    let prepared = [];
    for target in batch {
        let tc = target_cfg(target, cfg);
        try {
            let container = find_container(app_config(tc), tc.container);
            snapshot_current_tag(tc.app_id, tc.container, container);
            let image_name = if tc.contains("image_name") { tc.image_name } else { "" };
            let image_digest = if tc.contains("image_digest") { tc.image_digest } else { "" };
            let req = build_patch_request(tc, container.id, tc.image_tag, image_name, image_digest);
            prepared.push(#{
                ok: true, target: target, tc: tc,
                request: #{url: req.url, body: req.body, headers: auth_headers(tc.api_key)},
            });
        } catch (err) {
            prepared.push(#{ok: false, target: target, error: "" + err});
        }
    }

    let requests = [];
    for p in prepared {
        if p.ok {
            requests.push(p.request);
        }
    }
    let responses = if requests.len() > 0 { http_patch_all(requests) } else { [] };

    let succeeded = [];
    let failed = [];
    let response_i = 0;
    for p in prepared {
        if !p.ok {
            failed.push(#{app_id: p.target.app_id, error: p.error});
            continue;
        }
        let r = responses[response_i];
        response_i += 1;
        if r.status != 200 {
            failed.push(#{
                app_id: p.target.app_id,
                error: "bunny::patch_container: could not save container configuration: " +
                       "HTTP status " + r.status + "." + patch_failure_detail(r),
            });
            continue;
        }
        let ok = true;
        let error = "";
        try {
            let wait_cfg = p.tc;
            wait_cfg.attempts = fleet_health_attempts(cfg);
            wait_cfg.interval = fleet_health_interval(cfg);
            wait_for_image(wait_cfg);
            if p.target.contains("health_url") && p.target.health_url != "" {
                health::wait_healthy(p.target.health_url, #{
                    attempts: fleet_health_attempts(cfg), interval: fleet_health_interval(cfg),
                });
            }
        } catch (err) {
            ok = false;
            error = "" + err;
        }
        if ok {
            succeeded.push(p.target.app_id);
        } else {
            failed.push(#{app_id: p.target.app_id, error: error});
        }
    }

    #{succeeded: succeeded, failed: failed}
}

// deploy_fleet(targets, cfg) -> #{succeeded: Array<String>, failed: Array<#{app_id, error}>}
// Canary (targets[0..canary_size], sequential, fully verified) then the rest in concurrent
// batches of cfg.batch_size. Throws — naming every failed target, WITHOUT dispatching any
// further batch — the moment failed.len() exceeds cfg.max_failures. Never throws for a
// per-target failure below that threshold; that's the entire point of a threshold, not an
// all-or-nothing throw.
fn deploy_fleet(targets, cfg) {
    if targets.len() == 0 {
        throw "bunny::deploy_fleet: targets must not be empty.";
    }
    for i in 0..targets.len() {
        validate_target(i, targets[i]);
    }
    let canary_size = if cfg.contains("canary_size") { cfg.canary_size } else { 1 };
    let batch_size = if cfg.contains("batch_size") { cfg.batch_size } else { 5 };
    let max_failures = if cfg.contains("max_failures") { cfg.max_failures } else { 0 };
    if canary_size < 0 {
        throw "bunny::deploy_fleet: cfg.canary_size must be >= 0 (got " + canary_size + ").";
    }
    if batch_size < 1 {
        throw "bunny::deploy_fleet: cfg.batch_size must be >= 1 (got " + batch_size + ").";
    }
    if max_failures < 0 {
        throw "bunny::deploy_fleet: cfg.max_failures must be >= 0 (got " + max_failures + ").";
    }

    let succeeded = [];
    let failed = [];

    let canary_count = if canary_size > targets.len() { targets.len() } else { canary_size };

    // --- Canary: strictly sequential, fully verified before the rest of the fleet is touched. ---
    for i in 0..canary_count {
        let target = targets[i];
        let result = deploy_one_target_verified(target, cfg);
        if result.ok {
            succeeded.push(target.app_id);
        } else {
            failed.push(#{app_id: target.app_id, error: result.error});
        }
        if failed.len() > max_failures {
            throw "bunny::deploy_fleet: aborted after " + failed.len() + " failure(s) during " +
                  "canary (max_failures: " + max_failures + "): " + fleet_failure_summary(failed);
        }
    }

    // --- Batches: the rest, cfg.batch_size at a time, PATCHed concurrently within each batch. ---
    let i = canary_count;
    while i < targets.len() {
        let end = if i + batch_size > targets.len() { targets.len() } else { i + batch_size };
        let batch = [];
        for j in i..end {
            batch.push(targets[j]);
        }

        let batch_result = deploy_batch(batch, cfg);
        for s in batch_result.succeeded {
            succeeded.push(s);
        }
        for f in batch_result.failed {
            failed.push(f);
        }

        if failed.len() > max_failures {
            throw "bunny::deploy_fleet: aborted after " + failed.len() + " failure(s) " +
                  "(max_failures: " + max_failures + "): " + fleet_failure_summary(failed);
        }
        i = end;
    }

    #{succeeded: succeeded, failed: failed}
}
