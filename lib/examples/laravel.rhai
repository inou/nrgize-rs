// =============================================================================
// Energize.rhai — Laravel Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Laravel, built on the shared standard_deploy
// recipe (see the `import` line below). migrate + config/route/view cache run ONCE on the NEW
// image (pre_deploy), then a rolling deploy with a /up health check.
//
// Required secrets: REGISTRY_PASSWORD, APP_KEY, DATABASE_URL, DB_PASSWORD, DB_ROOT_PASSWORD
// Required env:     DEPLOY_TAG
// =============================================================================

import "lib/recipe" as recipe;

let SERVICE = "myapp";
let DB_HOST = "deploy@db.example.com";
let APP_PORT = 8000;   // 8000 for Octane, 9000 for FPM

recipe::standard_deploy(#{
    service:           SERVICE,
    image_repo:        "ghcr.io/myorg/myapp",
    registry:          "ghcr.io",
    registry_user:     env_or("REGISTRY_USER", "deploy"),
    registry_password: secret("REGISTRY_PASSWORD"),
    web_hosts:         ["deploy@web1.example.com", "deploy@web2.example.com"],
    db_host:           DB_HOST,
    port:              APP_PORT,
    health_path:       "/up",
    envs: #{
        "APP_ENV":          "production",
        "APP_DEBUG":        "false",
        "APP_KEY":          reveal(secret("APP_KEY")),
        "APP_URL":          env_or("APP_URL", "https://example.com"),
        "LOG_CHANNEL":      "stderr",
        "DATABASE_URL":     reveal(secret("DATABASE_URL")),
        "REDIS_HOST":       recipe::db_hostname(DB_HOST),
        "REDIS_PORT":       "6379",
        "CACHE_STORE":      "redis",
        "SESSION_DRIVER":   "redis",
        "QUEUE_CONNECTION": "redis",
    },
    // migrate + warm caches on the NEW image, once, before traffic switches.
    pre_deploy: "sh -c 'php artisan migrate --force && php artisan config:cache && php artisan route:cache && php artisan view:cache'",
    accessories: [
        #{ name: SERVICE + "-mysql", image: "mysql:8",
           ports: #{ "3306": "3306" },
           envs: #{ "MYSQL_DATABASE": SERVICE, "MYSQL_USER": SERVICE,
                    "MYSQL_PASSWORD": reveal(secret("DB_PASSWORD")),
                    "MYSQL_ROOT_PASSWORD": reveal(secret("DB_ROOT_PASSWORD")) },
           volumes: #{ "myapp-mysql-data": "/var/lib/mysql" } },
        #{ name: SERVICE + "-redis", image: "redis:7-alpine",
           ports: #{ "6379": "6379" }, volumes: #{ "myapp-redis-data": "/data" } },
    ],
});
