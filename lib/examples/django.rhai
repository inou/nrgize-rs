// =============================================================================
// Energize.rhai — Django Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Django applications.
//
// What this does:
//   1. Builds your Django Docker image locally
//   2. Pushes to your container registry
//   3. Pulls on all web servers
//   4. Runs manage.py migrate via the existing container
//   5. Collects static files
//   6. Rolling deploy with kamal-proxy traffic switching
//   7. Health check on /health/
//
// Prerequisites:
//   - Container runtime installed locally and on all hosts
//     (Docker, Podman, or OrbStack — auto-detected by default)
//   - SSH access to all hosts (key-based auth)
//   - Container registry credentials
//   - A Dockerfile in your Django project root
//
// Usage:
//   DEPLOY_TAG=v1.0.0 nrg exec
//   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
//
// Required secrets:
//   REGISTRY_PASSWORD  — Container registry token
//   DATABASE_URL       — Full postgres connection string
//   DJANGO_SECRET_KEY  — Django SECRET_KEY setting
//
// Dockerfile tips for Django:
//   - Use gunicorn or uvicorn as the production server
//   - Expose port 8000
//   - Add a /health/ endpoint (django-health-check or custom view)
//   - Collect static files in the build stage if using whitenoise
//   - Example CMD: gunicorn myapp.wsgi:application --bind 0.0.0.0:8000
//
// Health check endpoint (add to urls.py):
//   from django.http import JsonResponse
//   def health_check(request):
//       return JsonResponse({"status": "ok"})
//   urlpatterns = [ path("health/", health_check), ... ]
//
// SECRET NOTE: pass the raw Secret to registry_login (off-argv); reveal() secrets when
// placing them into an env map (the revealed plaintext stays registered for redaction).
// =============================================================================

import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;
import "lib/docker" as docker;

// Auto-detect container runtime (Docker, Podman, OrbStack, nerdctl).
rt::set_runtime("auto");

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

let SERVICE = "myapp";

// Container registry
let REGISTRY      = "ghcr.io";
let IMAGE_REPO    = "ghcr.io/myorg/myapp";
let REGISTRY_USER = env_or("REGISTRY_USER", "deploy");
let REGISTRY_PASS = secret("REGISTRY_PASSWORD");

let VERSION    = env_or("DEPLOY_TAG", "latest");
let FULL_IMAGE = IMAGE_REPO + ":" + VERSION;

// Hosts
let WEB_HOSTS = [
    "deploy@web1.example.com",
    "deploy@web2.example.com",
];

let DB_HOST = "deploy@db.example.com";

// Application settings
let APP_PORT = 8000;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

let DATABASE_URL  = reveal(secret("DATABASE_URL"));
let DJANGO_SECRET = reveal(secret("DJANGO_SECRET_KEY"));
let DB_HOSTNAME   = DB_HOST.split("@")[1];
let REDIS_URL     = env_or("REDIS_URL", "redis://" + DB_HOSTNAME + ":6379/0");

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

print("==> Deploying " + SERVICE + " " + VERSION);

// 1. Registry auth
print("\n--- Registry Login ---");
registry::registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS);
registry::registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS);

// 2. Accessories
print("\n--- Accessories ---");
deploy::accessory_run(DB_HOST, SERVICE + "-db", "postgres:16", #{
    ports:   #{ "5432": "5432" },
    envs:    #{
        "POSTGRES_DB":       SERVICE,
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
});

deploy::accessory_run(DB_HOST, SERVICE + "-redis", "redis:7-alpine", #{
    ports:   #{ "6379": "6379" },
    volumes: #{ "myapp-redis-data": "/data" },
});

// 3. Deploy
//    Migrate + collectstatic before switching traffic — joined with " && " via the `join` builtin.
let pre_deploy = join([
    rt::container_cmd() + " exec " + SERVICE + "-web python manage.py migrate --noinput 2>/dev/null || true",
    rt::container_cmd() + " exec " + SERVICE + "-web python manage.py collectstatic --noinput 2>/dev/null || true",
], " && ");

deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: APP_PORT,
    envs: #{
        "DJANGO_SETTINGS_MODULE": SERVICE + ".settings.production",
        "DATABASE_URL":           DATABASE_URL,
        "REDIS_URL":              REDIS_URL,
        "SECRET_KEY":             DJANGO_SECRET,
        "ALLOWED_HOSTS":          "*",
        "PYTHONUNBUFFERED":       "1",
    },
    health_path:     "/health/",
    health_attempts: 30,
    health_interval: 2,
    pre_deploy_cmd:  pre_deploy,
});

print("\n" + SERVICE + " " + VERSION + " is live.");
