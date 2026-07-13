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
// `containerTemplates` entry needs to describe — `current_image_tag` below ASSUMES
// `containerTemplates[i].imageTag` reflects the currently-deployed tag. Every doc/support page
// this research pass tried to fetch for independent confirmation returned HTTP 403. If your Bunny
// account's real GET response uses a different field name, fix it here (one line) rather than in
// every caller — `current_image_tag` is the only place this assumption lives.
//
// DRY-RUN: no explicit branching anywhere below. GET is already an honest, real read of existing
// reality even under `--dry-run` (see docs/builtins.md's `http_get` semantics), and PATCH already
// short-circuits under `--dry-run` to a synthetic 200 + a recorded `check` action — both inherited
// for free from Phase 1, exactly as the roadmap's Phase 2 line promises ("no new Rust needed").
// The one exception is `wait_for_image`'s polling loop, which has no live state to observe under
// dry-run (the PATCH never really ran) — it short-circuits via the global `is_dry_run()` builtin,
// the same escape hatch `lib/healthcheck.rhai::ssh_http_status` already established for the
// identical "can't observe a not-really-changed remote thing under dry-run" problem.

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
// cases for the same lookup).
fn find_container(app_config, container_name) {
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

// current_image_tag(cfg) -> string — convenience read. cfg: #{app_id, api_key, container}.
// Carries this file's flagged imageTag-field inference — see the header comment above.
fn current_image_tag(cfg) {
    let container = find_container(app_config(cfg), cfg.container);
    container.imageTag
}

// The shared PATCH path both deploy_app and rollback_app drive, given an ALREADY-RESOLVED
// container id (the caller has already done its own single app_config()+find_container() lookup
// — this function makes no GET of its own, so a full deploy_app/rollback_app call is exactly one
// GET + one PATCH, matching the real Bunny GitHub Action's own request shape exactly).
private fn patch_container(cfg, container_id, image_tag) {
    let body = #{id: container_id, imageTag: image_tag};
    if cfg.contains("image_name") && cfg.image_name != "" {
        body.imageName = cfg.image_name;
    }
    if cfg.contains("image_digest") && cfg.image_digest != "" {
        body.imageDigest = cfg.image_digest;
    }
    let url = base_url(cfg) + "/mc/apps/" + cfg.app_id + "/containers/" + container_id;
    let r = http_patch(url, to_json(body), auth_headers(cfg.api_key));
    if r.status != 200 {
        throw "bunny::patch_container: could not save container configuration: HTTP status " +
              r.status + ".";
    }
    r
}

private fn prev_key(app_id, container) {
    "bunny." + app_id + "." + container + ".prev"
}

// deploy_app(cfg) -> HttpResponse — upgrade one Bunny app's container to a new image.
// cfg: #{app_id, api_key, container, image_tag, image_name?, image_digest?}.
// Snapshots the CURRENT image tag into state (bunny.<app_id>.<container>.prev) before patching,
// so rollback_app(cfg) can revert it later — mirroring lib/deploy.rhai's own single-snapshot
// `<service>.prev` convention (see docs/roadmap.md's documented "rollback only ever targets the
// single snapshotted .prev" limitation — this module inherits the same, deliberate scope).
fn deploy_app(cfg) {
    let container = find_container(app_config(cfg), cfg.container);
    let current = container.imageTag;
    if current != () {
        state_set(prev_key(cfg.app_id, cfg.container), current);
    } else {
        print("  bunny::deploy_app: no current imageTag reported for \"" + cfg.container +
              "\" — skipping the rollback snapshot (nothing to revert to yet).");
    }
    patch_container(cfg, container.id, cfg.image_tag)
}

// rollback_app(cfg) -> HttpResponse — revert to the previously-deployed tag.
// cfg: #{app_id, api_key, container}. Throws if no previous image was ever snapshotted (i.e.
// deploy_app was never called for this app_id/container in this state store).
fn rollback_app(cfg) {
    let prev = state_get(prev_key(cfg.app_id, cfg.container));
    if prev == () {
        throw "bunny::rollback_app: no previous image recorded for app \"" + cfg.app_id +
              "\" container \"" + cfg.container + "\" — nothing to roll back to.";
    }
    let container = find_container(app_config(cfg), cfg.container);
    patch_container(cfg, container.id, prev)
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
