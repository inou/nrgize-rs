# =============================================================================
# Energize.star — Phoenix (Elixir) Deployment
# =============================================================================
#
# Zero-downtime Docker deployment for Phoenix applications using releases.
#
# What this does:
#   1. Builds your Phoenix Docker image locally (mix release)
#   2. Pushes to your container registry
#   3. Pulls on all web servers
#   4. Runs Ecto migrations via `bin/migrate`
#   5. Rolling deploy with kamal-proxy traffic switching
#   6. Health check on /health
#
# Prerequisites:
#   - Container runtime installed locally and on all hosts
#     (Docker, Podman, or OrbStack — auto-detected by default)
#   - SSH access to all hosts (key-based auth)
#   - Container registry credentials
#   - A Dockerfile using `mix release` (Phoenix 1.6+ generators include one)
#
# Usage:
#   DEPLOY_TAG=v1.0.0 nrg exec
#   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
#
# Required secrets:
#   REGISTRY_PASSWORD  — Container registry token
#   DATABASE_URL       — Full postgres connection string
#   SECRET_KEY_BASE    — Phoenix secret key base (min 64 chars)
#
# Health check endpoint (add to router.ex):
#   scope "/", MyAppWeb do
#     get "/health", HealthController, :index
#   end
#
#   # lib/my_app_web/controllers/health_controller.ex
#   defmodule MyAppWeb.HealthController do
#     use MyAppWeb, :controller
#     def index(conn, _params) do
#       json(conn, %{status: "ok"})
#     end
#   end
#
# Migration module (lib/my_app/release.ex):
#   defmodule MyApp.Release do
#     def migrate do
#       for repo <- repos() do
#         {:ok, _, _} = Ecto.Migrator.with_repo(repo, &Ecto.Migrator.run(&1, :up, all: true))
#       end
#     end
#     defp repos, do: Application.fetch_env!(:my_app, :ecto_repos)
#   end
#
# Dockerfile tips:
#   - Use `mix phx.gen.release --docker` to generate a production Dockerfile
#   - The release includes a `bin/migrate` script
#   - Ensure PHX_SERVER=true is set so the server starts
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
APP_PORT = 4000

# ---------------------------------------------------------------------------
# Secrets
# ---------------------------------------------------------------------------

DATABASE_URL    = secret("DATABASE_URL")
SECRET_KEY_BASE = secret("SECRET_KEY_BASE")

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
        "POSTGRES_DB":       SERVICE + "_prod",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": secret("DB_PASSWORD"),
    },
    volumes = {SERVICE + "-db-data": "/var/lib/postgresql/data"},
)

# 3. Deploy
deploy(
    hosts          = WEB_HOSTS,
    image          = FULL_IMAGE,
    service        = SERVICE,
    container_port = APP_PORT,
    envs = {
        "PHX_SERVER":       "true",
        "PHX_HOST":         env_or("PHX_HOST", "example.com"),
        "PORT":             str(APP_PORT),
        "DATABASE_URL":     DATABASE_URL,
        "SECRET_KEY_BASE":  SECRET_KEY_BASE,
        "MIX_ENV":          "prod",
        "POOL_SIZE":        env_or("POOL_SIZE", "10"),
    },
    health_path      = "/health",
    health_attempts  = 30,
    health_interval  = 2,
    # Run Ecto migrations via the release migrate script
    pre_deploy_cmd   = container_cmd() + " exec " + SERVICE + "-web bin/migrate 2>/dev/null || true",
)

print("\n" + SERVICE + " " + VERSION + " is live.")
