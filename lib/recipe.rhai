// recipe.rhai — one shared "standard deploy" recipe so the framework examples don't copy-paste
// the same registry-login + accessories + deploy flow five times (issue #22). Each example now
// supplies only the ~6 values that actually differ (port, health_path, envs, pre_deploy, the
// accessory list) and calls standard_deploy(cfg). Fixing the migration pattern or any shared bug
// happens HERE, once, instead of in five places.
//
// Usage (from an example Energize.rhai):
//   import "lib/recipe" as recipe;
//   recipe::standard_deploy(#{
//       service: "myapp",
//       image_repo: "ghcr.io/myorg/myapp",
//       registry: "ghcr.io",
//       registry_user: env_or("REGISTRY_USER", "deploy"),
//       registry_password: secret("REGISTRY_PASSWORD"),
//       web_hosts: ["deploy@web1", "deploy@web2"],
//       db_host: "deploy@db",
//       port: 3000,
//       health_path: "/up",
//       envs: #{ ... },                       // revealed secrets stay registered for redaction
//       pre_deploy: "bin/rails db:migrate",   // IN-CONTAINER release task, run on the NEW image
//       accessories: [ #{ name: "myapp-db", image: "postgres:16", ports: #{...}, envs: #{...} } ],
//   });

import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;

// standard_deploy's OWN cfg keys — consumed directly by THIS function, never forwarded to the
// wrapped `deploy::deploy()` call. Everything else in a caller's cfg map forwards automatically
// (see the denylist-based loop in standard_deploy below) — deliberately NOT a hand-maintained
// allowlist of deploy() keys, which the R12-addendum/R23c/R22 history above showed drifts out of
// sync every time deploy() grows a new cfg key (three separate incidents of the exact same bug
// class: a real deploy() key silently dropped by standard_deploy because this list was never
// updated). `port` is standard_deploy's own naming convenience for deploy()'s `container_port`
// (renamed, not forwarded under its own name); `accessories`/`db_host` configure the accessories
// step above, not the app's own deploy(); the rest are standard_deploy's required/registry/
// runtime-selection keys.
const STANDARD_DEPLOY_OWN_KEYS = [
    "service", "image_repo", "registry", "registry_user", "registry_password",
    "web_hosts", "db_host", "port", "version", "runtime", "accessories",
];

// standard_deploy(cfg) — registry login → accessories → zero-downtime rolling deploy.
fn standard_deploy(cfg) {
    // Robustness review R23: required keys used to be accessed directly (cfg.service,
    // cfg.image_repo, …) with no existence check — a caller who forgot one got no clear,
    // actionable message naming what's missing. What actually happened varied by key and how far
    // execution got before failing: e.g. a missing `image_repo` didn't error at all — Rhai's
    // missing-map-property read silently returns unit, so `cfg.image_repo + ":" + version` below
    // quietly built the image tag as literally ":latest", deploying it without complaint; other
    // missing keys instead surfaced as an opaque "Function not found" error deep inside an
    // unrelated module (e.g. registry/deploy/docker) once the malformed value reached a builtin
    // that couldn't accept it. Checked up front now, before anything (login, accessories, the
    // roll) runs, with a message naming exactly what's missing.
    for k in ["service", "image_repo", "web_hosts"] {
        if !cfg.contains(k) {
            throw "standard_deploy: missing required cfg key '" + k + "' (robustness review R23).";
        }
    }
    if cfg.contains("registry") {
        for k in ["registry_user", "registry_password"] {
            if !cfg.contains(k) {
                throw "standard_deploy: cfg.registry is set but '" + k + "' is missing " +
                      "(robustness review R23).";
            }
        }
    }
    if cfg.contains("accessories") && !cfg.accessories.is_empty() && !cfg.contains("db_host") {
        throw "standard_deploy: cfg.accessories is set but 'db_host' is missing " +
              "(robustness review R23).";
    }

    let runtime = if cfg.contains("runtime") { cfg.runtime } else { "auto" };
    rt::set_runtime(runtime);

    let service = cfg.service;
    let version = if cfg.contains("version") { cfg.version } else { env_or("DEPLOY_TAG", "latest") };
    let full_image = cfg.image_repo + ":" + version;

    print("==> Deploying " + service + " " + version);

    // 1. Registry authentication (the Secret streams to --password-stdin, off-argv).
    if cfg.contains("registry") {
        print("\n--- Registry Login ---");
        registry::registry_login("local", cfg.registry, cfg.registry_user, cfg.registry_password);
        registry::registry_login_all(cfg.web_hosts, cfg.registry, cfg.registry_user, cfg.registry_password);
        // Fable final review (roadmap 1.1 step 3a): with cfg.build_host set, the build (and the
        // registry push) run THERE instead of locally — that host needs its own registry session
        // too, for a private BASE image pull during the build as well as the push afterward.
        // Without this, the feature's most common real configuration (a private registry +
        // cfg.build_host) failed live at the push with an opaque "unauthorized", with nothing
        // having warned the caller why.
        if cfg.contains("build_host") && cfg.build_host != "" {
            registry::registry_login(cfg.build_host, cfg.registry, cfg.registry_user, cfg.registry_password);
        }
    }

    // 2. Accessories (databases, cache, …) on the db host.
    if cfg.contains("accessories") && !cfg.accessories.is_empty() {
        print("\n--- Accessories ---");
        for a in cfg.accessories {
            // Robustness review R23 (found during this fix's own review): each accessory map's
            // OWN required keys (name, image) were still accessed directly, one level below the
            // top-level cfg keys this fix already guards — the same class of unclear failure
            // (silent malformed values, or an opaque error deep in an unrelated module) just
            // moved one map deeper instead of eliminated.
            for k in ["name", "image"] {
                if !a.contains(k) {
                    throw "standard_deploy: an entry in cfg.accessories is missing required key '" +
                          k + "' (robustness review R23).";
                }
            }
            let acfg = #{
                ports:   if a.contains("ports") { a.ports } else { #{} },
                envs:    if a.contains("envs") { a.envs } else { #{} },
                volumes: if a.contains("volumes") { a.volumes } else { #{} },
            };
            // Robustness review R23: cfg.network was already forwarded to the app's own deploy()
            // below, but NOT to accessories — so a caller on a custom Docker network got their app
            // container joined to it while the DB/cache stayed on the default bridge network,
            // unable to resolve each other by container name.
            if cfg.contains("network") { acfg.network = cfg.network; }
            deploy::accessory_run(cfg.db_host, a.name, a.image, acfg);
        }
    }

    // 3. Zero-downtime rolling deploy. pre_deploy (e.g. migrations) runs ONCE on the NEW image.
    //
    // Structural refactor (suggested during R23c's own final review, implemented now): every
    // prior version of this forwarding step was an ALLOWLIST — a fixed list of deploy() cfg keys,
    // hand-copied here and re-copied every time deploy() grew a new one. That drifted out of sync
    // three separate times (R12's health_consecutive/health_timeout, R23c's nine more keys, R22's
    // keep_images), each one a real caller-facing bug: a cfg key silently ignored with no error.
    // A DENYLIST inverts this — forward every cfg key EXCEPT standard_deploy's own ~11
    // (`STANDARD_DEPLOY_OWN_KEYS` above) — so any FUTURE deploy() cfg key forwards automatically,
    // with no change needed here. deploy() itself already supplies every default (container_port,
    // envs, health_path, ...) for whatever's absent from dcfg, so nothing needs re-defaulting here.
    let dcfg = #{};
    for k in cfg.keys() {
        if !global::STANDARD_DEPLOY_OWN_KEYS.contains(k) {
            dcfg[k] = cfg[k];
        }
    }
    // `port` is standard_deploy's own naming convenience, renamed to deploy()'s
    // `container_port` — it's on the denylist above precisely so it does NOT ALSO forward
    // under its own name (deploy() has no `port` key of its own).
    if cfg.contains("port") { dcfg.container_port = cfg.port; }

    deploy::deploy(cfg.web_hosts, full_image, service, dcfg);

    print("\n" + service + " " + version + " is live.");
}

// db_hostname(host) — the hostname part of a "user@host" (or a bare "host"), WITHOUT panicking
// when there is no "@". The old examples used `DB_HOST.split("@")[1]`, which index-panics for a
// plain hostname; this helper is safe.
fn db_hostname(host) {
    let parts = host.split("@");
    if parts.len() > 1 { parts[parts.len() - 1] } else { host }
}
