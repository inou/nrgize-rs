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

// standard_deploy(cfg) — registry login → accessories → zero-downtime rolling deploy.
fn standard_deploy(cfg) {
    // Robustness review R23: required keys used to be accessed directly (cfg.service,
    // cfg.image_repo, …) — a caller who forgot one saw an opaque Rhai "property not found" error
    // deep inside this function's own body instead of a clear, actionable message naming exactly
    // what's missing. Checked up front, before anything (login, accessories, the roll) runs.
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
    }

    // 2. Accessories (databases, cache, …) on the db host.
    if cfg.contains("accessories") && !cfg.accessories.is_empty() {
        print("\n--- Accessories ---");
        for a in cfg.accessories {
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
    let dcfg = #{
        container_port: if cfg.contains("port") { cfg.port } else { 3000 },
        envs:           if cfg.contains("envs") { cfg.envs } else { #{} },
        health_path:    if cfg.contains("health_path") { cfg.health_path } else { "/up" },
    };
    if cfg.contains("pre_deploy") { dcfg.pre_deploy = cfg.pre_deploy; }
    if cfg.contains("network")    { dcfg.network = cfg.network; }
    if cfg.contains("proxy")      { dcfg.proxy = cfg.proxy; }
    if cfg.contains("domain")     { dcfg.domain = cfg.domain; }

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
