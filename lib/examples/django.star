# =============================================================================
# Energize.star — Django Deployment
# =============================================================================
#
# Zero-downtime Docker deployment for Django applications.
#
# What this does:
#   1. Builds your Django Docker image locally
#   2. Pushes to your container registry
#   3. Pulls on all web servers
#   4. Runs manage.py migrate via the existing container
#   5. Collects static files
#   6. Rolling deploy with kamal-proxy traffic switching
#   7. Health check on /health/
#
# Prerequisites:
#   - Container runtime installed locally and on all hosts
#     (Docker, Podman, or OrbStack — auto-detected by default)
#   - SSH access to all hosts (key-based auth)
#   - Container registry credentials
#   - A Dockerfile in your Django project root
#
# Usage:
#   DEPLOY_TAG=v1.0.0 nrg exec
#   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
#
# Required secrets:
#   REGISTRY_PASSWORD  — Container registry token
#   DATABASE_URL       — Full postgres connection string
#   DJANGO_SECRET_KEY  — Django SECRET_KEY setting
#
# Dockerfile tips for Django:
#   - Use gunicorn or uvicorn as the production server
#   - Expose port 8000
#   - Add a /health/ endpoint (django-health-check or custom view)
#   - Collect static files in the build stage if using whitenoise
#   - Example CMD: gunicorn myapp.wsgi:application --bind 0.0.0.0:8000
#
# Health check endpoint (add to urls.py):
#   from django.http import JsonResponse
#   def health_check(request):
#       return JsonResponse({"status": "ok"})
#   urlpatterns = [
#       path("health/", health_check),
#       ...
#   ]
#
# =============================================================================

load("lib/runtime.star", "set_runtime", "container_cmd")
load("lib/deploy.star", "deploy", "rollback", "accessory_run")
load("lib/registry.star", "registry_login", "registry_login_all")
load("lib/docker.star", "docker_exec")

# Auto-detect container runtime (Docker, Podman, OrbStack, nerdctl).
# To force a specific runtime: set_runtime("podman") or set_runtime("docker")
set_runtime("auto")

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

SERVICE = "myapp"

# Container registry
REGISTRY        = "ghcr.io"
IMAGE_REPO      = "ghcr.io/myorg/myapp"
REGISTRY_USER   = env_or("REGISTRY_USER", "deploy")
REGISTRY_PASS   = secret("REGISTRY_PASSWORD")

VERSION    = env_or("DEPLOY_TAG", "latest")
FULL_IMAGE = IMAGE_REPO + ":" + VERSION

# Hosts
WEB_HOSTS = [
    "deploy@web1.example.com",
    "deploy@web2.example.com",
]

DB_HOST = "deploy@db.example.com"

# Application settings
APP_PORT = 8000

# ---------------------------------------------------------------------------
# Secrets
# ---------------------------------------------------------------------------

DATABASE_URL     = secret("DATABASE_URL")
DJANGO_SECRET    = secret("DJANGO_SECRET_KEY")
REDIS_URL        = env_or("REDIS_URL", "redis://" + DB_HOST.split("@")[1] + ":6379/0")

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

print("==> Deploying " + SERVICE + " " + VERSION)

# 1. Registry auth
print("\n--- Registry Login ---")
registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS)
registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS)

# 2. Accessories
print("\n--- Accessories ---")
accessory_run(
    host    = DB_HOST,
    name    = SERVICE + "-db",
    image   = "postgres:16",
    ports   = {"5432": "5432"},
    envs    = {
        "POSTGRES_DB":       SERVICE,
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": secret("DB_PASSWORD"),
    },
    volumes = {SERVICE + "-db-data": "/var/lib/postgresql/data"},
)

accessory_run(
    host    = DB_HOST,
    name    = SERVICE + "-redis",
    image   = "redis:7-alpine",
    ports   = {"6379": "6379"},
    volumes = {SERVICE + "-redis-data": "/data"},
)

# 3. Deploy
deploy(
    hosts          = WEB_HOSTS,
    image          = FULL_IMAGE,
    service        = SERVICE,
    container_port = APP_PORT,
    envs = {
        "DJANGO_SETTINGS_MODULE": SERVICE + ".settings.production",
        "DATABASE_URL":           DATABASE_URL,
        "REDIS_URL":              REDIS_URL,
        "SECRET_KEY":             DJANGO_SECRET,
        "ALLOWED_HOSTS":          "*",
        "PYTHONUNBUFFERED":       "1",
    },
    health_path      = "/health/",
    health_attempts  = 30,
    health_interval  = 2,
    # Migrate + collectstatic before switching traffic
    pre_deploy_cmd   = " && ".join([
        container_cmd() + " exec " + SERVICE + "-web python manage.py migrate --noinput 2>/dev/null || true",
        container_cmd() + " exec " + SERVICE + "-web python manage.py collectstatic --noinput 2>/dev/null || true",
    ]),
)

print("\n" + SERVICE + " " + VERSION + " is live.")
