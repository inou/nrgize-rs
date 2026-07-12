// =============================================================================
// Energize.rhai — Django Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Django, built on the shared standard_deploy
// recipe (see the `import` line below). Migrate + collectstatic run ONCE on the NEW image
// (pre_deploy), then a rolling deploy with a /health/ health check.
//
// Required secrets: REGISTRY_PASSWORD, DATABASE_URL, DJANGO_SECRET_KEY, DB_PASSWORD
// Required env:     DEPLOY_TAG
//
// Health endpoint (urls.py):
//   def health_check(request): return JsonResponse({"status": "ok"})
//   urlpatterns = [ path("health/", health_check), ... ]
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
    port:              8000,
    health_path:       "/health/",
    envs: #{
        "DJANGO_SETTINGS_MODULE": SERVICE + ".settings.production",
        "DATABASE_URL":           reveal(secret("DATABASE_URL")),
        "REDIS_URL":              env_or("REDIS_URL", "redis://" + recipe::db_hostname(DB_HOST) + ":6379/0"),
        "SECRET_KEY":             reveal(secret("DJANGO_SECRET_KEY")),
        "ALLOWED_HOSTS":          "*",
        "PYTHONUNBUFFERED":       "1",
    },
    // Migrate + collectstatic on the NEW image, once, before traffic switches.
    pre_deploy: "sh -c 'python manage.py migrate --noinput && python manage.py collectstatic --noinput'",
    accessories: [
        #{ name: SERVICE + "-db", image: "postgres:16",
           ports: #{ "5432": "5432" },
           envs: #{ "POSTGRES_DB": SERVICE, "POSTGRES_USER": SERVICE,
                    "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")) },
           volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" } },
        #{ name: SERVICE + "-redis", image: "redis:7-alpine",
           ports: #{ "6379": "6379" }, volumes: #{ "myapp-redis-data": "/data" } },
    ],
});
