// =============================================================================
// Energize.rhai — Phoenix (Elixir) Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Phoenix, built on the shared standard_deploy
// recipe (lib/recipe.rhai). Ecto migrations run ONCE on the NEW image via the
// release migrate script, then a rolling deploy with a /health health check.
//
// Required secrets: REGISTRY_PASSWORD, DATABASE_URL, SECRET_KEY_BASE, DB_PASSWORD
// Required env:     DEPLOY_TAG
// =============================================================================

import "lib/recipe" as recipe;

let SERVICE = "myapp";
let DB_HOST = "deploy@db.example.com";
let APP_PORT = 4000;

recipe::standard_deploy(#{
    service:           SERVICE,
    image_repo:        "ghcr.io/myorg/myapp",
    registry:          "ghcr.io",
    registry_user:     env_or("REGISTRY_USER", "deploy"),
    registry_password: secret("REGISTRY_PASSWORD"),
    web_hosts:         ["deploy@web1.example.com", "deploy@web2.example.com"],
    db_host:           DB_HOST,
    port:              APP_PORT,
    health_path:       "/health",
    envs: #{
        "PHX_SERVER":      "true",
        "PHX_HOST":        env_or("PHX_HOST", "example.com"),
        "PORT":            APP_PORT.to_string(),
        "DATABASE_URL":    reveal(secret("DATABASE_URL")),
        "SECRET_KEY_BASE": reveal(secret("SECRET_KEY_BASE")),
        "MIX_ENV":         "prod",
        "POOL_SIZE":       env_or("POOL_SIZE", "10"),
    },
    // The release's migrate entrypoint, on the NEW image, once.
    pre_deploy: "bin/migrate",
    accessories: [
        #{ name: SERVICE + "-db", image: "postgres:16",
           ports: #{ "5432": "5432" },
           envs: #{ "POSTGRES_DB": SERVICE + "_prod", "POSTGRES_USER": SERVICE,
                    "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")) },
           volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" } },
    ],
});
