# Energize.star — Example deployment configuration for a Rails/Phoenix/Node app.
#
# This is the Kamal-equivalent: a single file that defines your entire
# deployment workflow. Run with: nrg exec
#
# Customize the variables below, add your hosts, and you have a production
# deployment pipeline.

load("lib/runtime.star", "set_runtime", "container_cmd")
load("lib/deploy.star", "deploy", "rollback", "accessory_run")
load("lib/registry.star", "registry_login", "registry_login_all")
load("lib/docker.star", "docker_exec")

# Auto-detect container runtime (Docker, Podman, OrbStack, nerdctl)
set_runtime("auto")

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

SERVICE    = "myapp"
IMAGE      = "ghcr.io/myorg/myapp"
VERSION    = env_or("DEPLOY_TAG", "latest")
FULL_IMAGE = IMAGE + ":" + VERSION

WEB_HOSTS  = [
    "deploy@10.0.0.1",
    "deploy@10.0.0.2",
]

DB_HOST = "deploy@10.0.0.3"

REGISTRY_SERVER   = "ghcr.io"
REGISTRY_USER     = env_or("REGISTRY_USER", "deploy")
REGISTRY_PASSWORD = secret("REGISTRY_PASSWORD")

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

# 1. Authenticate with the registry everywhere
print("==> Registry login")
registry_login("local", REGISTRY_SERVER, REGISTRY_USER, REGISTRY_PASSWORD)
registry_login_all(WEB_HOSTS, REGISTRY_SERVER, REGISTRY_USER, REGISTRY_PASSWORD)

# 2. Ensure accessories are running
print("\n==> Accessories")
accessory_run(
    host    = DB_HOST,
    name    = SERVICE + "-db",
    image   = "postgres:16",
    ports   = {"5432": "5432"},
    envs    = {
        "POSTGRES_DB":       SERVICE + "_production",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": secret("DB_PASSWORD"),
    },
    volumes = {SERVICE + "-db-data": "/var/lib/postgresql/data"},
)

accessory_run(
    host    = DB_HOST,
    name    = SERVICE + "-redis",
    image   = "redis:7",
    ports   = {"6379": "6379"},
    volumes = {SERVICE + "-redis-data": "/data"},
)

# 3. Deploy web service with zero-downtime rolling update
deploy(
    hosts          = WEB_HOSTS,
    image          = FULL_IMAGE,
    service        = SERVICE,
    container_port = 3000,
    envs = {
        "DATABASE_URL":     "postgres://" + SERVICE + ":" + secret("DB_PASSWORD") + "@" + DB_HOST + ":5432/" + SERVICE + "_production",
        "REDIS_URL":        "redis://" + DB_HOST + ":6379/0",
        "RAILS_ENV":        "production",
        "SECRET_KEY_BASE":  secret("SECRET_KEY_BASE"),
    },
    health_path    = "/up",
    pre_deploy_cmd = container_cmd() + " exec " + SERVICE + "-web bin/rails db:migrate 2>/dev/null || true",
)

print("\nDone! " + SERVICE + " " + VERSION + " is live.")
