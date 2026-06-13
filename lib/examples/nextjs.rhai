// =============================================================================
// Energize.rhai — Next.js Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Next.js, built on the shared standard_deploy
// recipe (lib/recipe.rhai). The DB + DATABASE_URL are OPTIONAL (only if you use a
// database for API routes). Health check on /api/health.
//
// Required secrets: REGISTRY_PASSWORD (and DB_PASSWORD if you enable the accessory)
// Required env:     DEPLOY_TAG
// =============================================================================

import "lib/recipe" as recipe;

let SERVICE = "myapp";
let DB_HOST = "deploy@db.example.com";
let APP_PORT = 3000;

// Optional DB: enable the accessory + DATABASE_URL only when DATABASE_URL is set.
let DATABASE_URL = env_or("DATABASE_URL", "");

let envs = #{
    "NODE_ENV": "production",
    "HOSTNAME": "0.0.0.0",
    "PORT":     APP_PORT.to_string(),
};
if DATABASE_URL != "" { envs["DATABASE_URL"] = DATABASE_URL; }
let NEXT_PUBLIC_URL = env_or("NEXT_PUBLIC_URL", "");
if NEXT_PUBLIC_URL != "" { envs["NEXT_PUBLIC_URL"] = NEXT_PUBLIC_URL; }

let accessories = [];
if DATABASE_URL != "" {
    accessories.push(#{
        name: SERVICE + "-db", image: "postgres:16",
        ports: #{ "5432": "5432" },
        envs: #{ "POSTGRES_DB": SERVICE, "POSTGRES_USER": SERVICE,
                 "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")) },
        volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
    });
}

recipe::standard_deploy(#{
    service:           SERVICE,
    image_repo:        "ghcr.io/myorg/myapp",
    registry:          "ghcr.io",
    registry_user:     env_or("REGISTRY_USER", "deploy"),
    registry_password: secret("REGISTRY_PASSWORD"),
    web_hosts:         ["deploy@web1.example.com", "deploy@web2.example.com"],
    db_host:           DB_HOST,
    port:              APP_PORT,
    health_path:       "/api/health",
    envs:              envs,
    accessories:       accessories,
    // Next.js usually has no migrations. For Prisma, uncomment:
    // pre_deploy: "npx prisma migrate deploy",
});
