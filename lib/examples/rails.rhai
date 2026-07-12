// =============================================================================
// Energize.rhai — Ruby on Rails Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Rails, built on the shared standard_deploy
// recipe (see the `import` line below). Only the Rails-specific values live here; the common
// registry-login + accessories + rolling-deploy flow is in the recipe.
//
//   1. Build/push happen in your CI (or set skip_build/skip_push false to build here)
//   2. Pull on all web servers
//   3. Run db:migrate ONCE in a throwaway container built on the NEW image (pre_deploy)
//   4. Rolling deploy with kamal-proxy traffic switching + /up health check
//   5. Clean up old containers and images
//
// Required secrets (env or .env / .energize/secrets):
//   REGISTRY_PASSWORD, DATABASE_URL, SECRET_KEY_BASE, DB_PASSWORD
// Required env:  DEPLOY_TAG (image tag, e.g. a git SHA)
//
// SECRET NOTE: pass the raw Secret to the recipe's registry_password (off-argv
// --password-stdin); reveal() secrets only when placing them into the env map.
// =============================================================================

import "lib/recipe" as recipe;

let SERVICE = "myapp";
let DB_HOST = "deploy@db.example.com";

recipe::standard_deploy(#{
    service:           SERVICE,
    image_repo:        "ghcr.io/myorg/myapp",
    registry:          "ghcr.io",
    registry_user:     env_or("REGISTRY_USER", "deploy"),
    registry_password: secret("REGISTRY_PASSWORD"),
    web_hosts:         ["deploy@web1.example.com", "deploy@web2.example.com"],
    db_host:           DB_HOST,
    port:              3000,
    health_path:       "/up",   // Rails 7.1+ ships /up by default
    envs: #{
        "RAILS_ENV":                "production",
        "RAILS_LOG_TO_STDOUT":      "1",
        "RAILS_SERVE_STATIC_FILES": "1",
        "DATABASE_URL":             reveal(secret("DATABASE_URL")),
        "REDIS_URL":                env_or("REDIS_URL", "redis://" + recipe::db_hostname(DB_HOST) + ":6379/0"),
        "SECRET_KEY_BASE":          reveal(secret("SECRET_KEY_BASE")),
    },
    // Migrations run ONCE on the NEW image, before any traffic switches; a failure aborts.
    pre_deploy: "bin/rails db:migrate",
    accessories: [
        #{ name: SERVICE + "-db", image: "postgres:16",
           ports: #{ "5432": "5432" },
           envs: #{ "POSTGRES_DB": SERVICE + "_production", "POSTGRES_USER": SERVICE,
                    "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")) },
           volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" } },
        #{ name: SERVICE + "-redis", image: "redis:7-alpine",
           ports: #{ "6379": "6379" }, volumes: #{ "myapp-redis-data": "/data" } },
    ],
});
