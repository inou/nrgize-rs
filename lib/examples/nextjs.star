# =============================================================================
# Energize.star — Next.js Deployment
# =============================================================================
#
# Zero-downtime Docker deployment for Next.js applications.
#
# What this does:
#   1. Builds your Next.js Docker image locally (standalone output)
#   2. Pushes to your container registry
#   3. Pulls on all web servers
#   4. Rolling deploy with kamal-proxy traffic switching
#   5. Health check on /api/health
#
# Prerequisites:
#   - Container runtime installed locally and on all hosts
#     (Docker, Podman, or OrbStack — auto-detected by default)
#   - SSH access to all hosts (key-based auth)
#   - Container registry credentials
#   - next.config.js with `output: "standalone"`
#
# Usage:
#   DEPLOY_TAG=v1.0.0 nrg exec
#   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
#
# Required secrets:
#   REGISTRY_PASSWORD  — Container registry token
#   DATABASE_URL       — Database connection string (if using a DB)
#
# next.config.js setup:
#   module.exports = {
#     output: "standalone",  // Required for Docker
#   }
#
# Health check API route (app/api/health/route.ts):
#   export async function GET() {
#     return Response.json({ status: "ok" });
#   }
#
# Recommended Dockerfile (multi-stage):
#   FROM node:20-alpine AS deps
#   WORKDIR /app
#   COPY package.json package-lock.json ./
#   RUN npm ci
#
#   FROM node:20-alpine AS builder
#   WORKDIR /app
#   COPY --from=deps /app/node_modules ./node_modules
#   COPY . .
#   RUN npm run build
#
#   FROM node:20-alpine AS runner
#   WORKDIR /app
#   ENV NODE_ENV=production
#   COPY --from=builder /app/.next/standalone ./
#   COPY --from=builder /app/.next/static ./.next/static
#   COPY --from=builder /app/public ./public
#   EXPOSE 3000
#   CMD ["node", "server.js"]
#
# =============================================================================

load("lib/runtime.star", "set_runtime", "container_cmd")
load("lib/deploy.star", "deploy", "rollback", "accessory_run")
load("lib/registry.star", "registry_login", "registry_login_all")

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
APP_PORT = 3000

# ---------------------------------------------------------------------------
# Secrets
# ---------------------------------------------------------------------------

DATABASE_URL = env_or("DATABASE_URL", "")

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

print("==> Deploying " + SERVICE + " " + VERSION)

# 1. Registry auth
print("\n--- Registry Login ---")
registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS)
registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS)

# 2. Accessories (optional — only if you need a DB for Next.js API routes)
if DATABASE_URL:
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

# 3. Build environment variables
container_envs = {
    "NODE_ENV":     "production",
    "HOSTNAME":     "0.0.0.0",
    "PORT":         str(APP_PORT),
}

# Add optional env vars
if DATABASE_URL:
    container_envs["DATABASE_URL"] = DATABASE_URL

NEXT_PUBLIC_URL = env_or("NEXT_PUBLIC_URL", "")
if NEXT_PUBLIC_URL:
    container_envs["NEXT_PUBLIC_URL"] = NEXT_PUBLIC_URL

# 4. Deploy
deploy(
    hosts          = WEB_HOSTS,
    image          = FULL_IMAGE,
    service        = SERVICE,
    container_port = APP_PORT,
    envs           = container_envs,
    health_path    = "/api/health",
    health_attempts = 30,
    health_interval = 2,
    # Next.js typically has no migrations, but you can run prisma migrate here
    # pre_deploy_cmd = container_cmd() + " exec " + SERVICE + "-web npx prisma migrate deploy 2>/dev/null || true",
)

print("\n" + SERVICE + " " + VERSION + " is live.")
