# =============================================================================
# Energize.star — Laravel Deployment
# =============================================================================
#
# Zero-downtime Docker deployment for Laravel applications.
#
# What this does:
#   1. Builds your Laravel Docker image locally
#   2. Pushes to your container registry
#   3. Pulls on all web servers
#   4. Runs artisan migrate, config/route/view cache
#   5. Rolling deploy with kamal-proxy traffic switching
#   6. Health check on /up (Laravel 11+ default)
#
# Prerequisites:
#   - Container runtime installed locally and on all hosts
#     (Docker, Podman, or OrbStack — auto-detected by default)
#   - SSH access to all hosts (key-based auth)
#   - Container registry credentials
#   - A Dockerfile for your Laravel project
#
# Usage:
#   DEPLOY_TAG=v1.0.0 nrg exec
#   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
#
# Required secrets:
#   REGISTRY_PASSWORD  — Container registry token
#   DATABASE_URL       — Full mysql/postgres connection string
#   APP_KEY            — Laravel APP_KEY (base64:...)
#
# Health check:
#   Laravel 11+ includes /up by default.
#   For older versions, add a route:
#     Route::get('/up', fn() => response()->json(['status' => 'ok']));
#
# Dockerfile tips for Laravel:
#   - Use PHP 8.3+ with FPM or a Swoole/FrankenPHP/Octane server
#   - Install composer dependencies with --no-dev --optimize-autoloader
#   - Copy .env or pass all config via environment variables
#   - Run `php artisan config:cache` in the build stage for faster boot
#   - Expose port 8000 (Octane) or 9000 (FPM behind nginx)
#
# Recommended Dockerfile (FrankenPHP / Laravel Octane):
#   FROM dunglas/frankenphp:latest-php8.3-alpine
#   WORKDIR /app
#   COPY composer.json composer.lock ./
#   RUN composer install --no-dev --optimize-autoloader --no-scripts
#   COPY . .
#   RUN php artisan config:cache && \
#       php artisan route:cache && \
#       php artisan view:cache
#   EXPOSE 8000
#   CMD ["php", "artisan", "octane:start", "--server=frankenphp", "--host=0.0.0.0", "--port=8000"]
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

# Application settings — 8000 for Octane, 9000 for FPM
APP_PORT = 8000

# ---------------------------------------------------------------------------
# Secrets
# ---------------------------------------------------------------------------

APP_KEY      = secret("APP_KEY")
DATABASE_URL = secret("DATABASE_URL")

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
    name    = SERVICE + "-mysql",
    image   = "mysql:8",
    ports   = {"3306": "3306"},
    envs    = {
        "MYSQL_DATABASE":      SERVICE,
        "MYSQL_USER":          SERVICE,
        "MYSQL_PASSWORD":      secret("DB_PASSWORD"),
        "MYSQL_ROOT_PASSWORD": secret("DB_ROOT_PASSWORD"),
    },
    volumes = {SERVICE + "-mysql-data": "/var/lib/mysql"},
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
        "APP_ENV":          "production",
        "APP_DEBUG":        "false",
        "APP_KEY":          APP_KEY,
        "APP_URL":          env_or("APP_URL", "https://example.com"),
        "LOG_CHANNEL":      "stderr",
        "DATABASE_URL":     DATABASE_URL,
        "REDIS_HOST":       DB_HOST.split("@")[1],
        "REDIS_PORT":       "6379",
        "CACHE_STORE":      "redis",
        "SESSION_DRIVER":   "redis",
        "QUEUE_CONNECTION": "redis",
    },
    health_path      = "/up",
    health_attempts  = 30,
    health_interval  = 2,
    # Migrate + cache before switching traffic
    pre_deploy_cmd   = " && ".join([
        container_cmd() + " exec " + SERVICE + "-web php artisan migrate --force 2>/dev/null || true",
        container_cmd() + " exec " + SERVICE + "-web php artisan config:cache 2>/dev/null || true",
        container_cmd() + " exec " + SERVICE + "-web php artisan route:cache 2>/dev/null || true",
        container_cmd() + " exec " + SERVICE + "-web php artisan view:cache 2>/dev/null || true",
    ]),
)

print("\n" + SERVICE + " " + VERSION + " is live.")

# ---------------------------------------------------------------------------
# Queue Worker (optional — uncomment if you use queues)
# ---------------------------------------------------------------------------
#
# To run a dedicated queue worker alongside your web containers, add a
# separate deploy for the worker process. Workers don't need proxy routing,
# so you can use docker_run directly:
#
# load("lib/docker.star", "docker_run", "docker_stop", "docker_remove", "docker_pull")
#
# for host in WEB_HOSTS:
#     docker_pull(host, FULL_IMAGE)
#     docker_stop(host, SERVICE + "-worker")
#     docker_remove(host, SERVICE + "-worker")
#     docker_run(
#         host = host,
#         tag  = FULL_IMAGE,
#         name = SERVICE + "-worker",
#         envs = { ... same as above ... },
#         extra = "php artisan queue:work --sleep=3 --tries=3 --max-time=3600",
#     )
