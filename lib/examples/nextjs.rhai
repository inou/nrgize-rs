// =============================================================================
// Energize.rhai — Next.js Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Next.js applications.
//
// What this does:
//   1. Builds your Next.js Docker image locally (standalone output)
//   2. Pushes to your container registry
//   3. Pulls on all web servers
//   4. Rolling deploy with kamal-proxy traffic switching
//   5. Health check on /api/health
//
// Prerequisites:
//   - Container runtime installed locally and on all hosts
//     (Docker, Podman, or OrbStack — auto-detected by default)
//   - SSH access to all hosts (key-based auth)
//   - Container registry credentials
//   - next.config.js with `output: "standalone"`
//
// Usage:
//   DEPLOY_TAG=v1.0.0 nrg exec
//   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
//
// Required secrets:
//   REGISTRY_PASSWORD  — Container registry token
//   DATABASE_URL       — Database connection string (if using a DB)
//
// next.config.js setup:
//   module.exports = { output: "standalone" };  // Required for Docker
//
// Health check API route (app/api/health/route.ts):
//   export async function GET() { return Response.json({ status: "ok" }); }
//
// SECRET NOTE: pass the raw Secret to registry_login (off-argv); reveal() secrets when
// placing them into an env map (the revealed plaintext stays registered for redaction).
// =============================================================================

import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;

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
let APP_PORT = 3000;

// ---------------------------------------------------------------------------
// Secrets — DATABASE_URL is optional for Next.js (only if you use a DB).
// ---------------------------------------------------------------------------

let DATABASE_URL = env_or("DATABASE_URL", "");

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

print("==> Deploying " + SERVICE + " " + VERSION);

// 1. Registry auth
print("\n--- Registry Login ---");
registry::registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS);
registry::registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS);

// 2. Accessories (optional — only if you need a DB for Next.js API routes).
//    Rhai has no truthiness on strings: test `!= ""` explicitly.
if DATABASE_URL != "" {
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
}

// 3. Build environment variables. `str(x)` becomes `x.to_string()` in Rhai.
let container_envs = #{
    "NODE_ENV": "production",
    "HOSTNAME": "0.0.0.0",
    "PORT":     APP_PORT.to_string(),
};

// Add optional env vars (guard with `!= ""` — Rhai has no string truthiness).
if DATABASE_URL != "" {
    container_envs["DATABASE_URL"] = DATABASE_URL;
}

let NEXT_PUBLIC_URL = env_or("NEXT_PUBLIC_URL", "");
if NEXT_PUBLIC_URL != "" {
    container_envs["NEXT_PUBLIC_URL"] = NEXT_PUBLIC_URL;
}

// 4. Deploy
deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port:  APP_PORT,
    envs:            container_envs,
    health_path:     "/api/health",
    health_attempts: 30,
    health_interval: 2,
    // Next.js typically has no migrations, but you can run prisma migrate here:
    // pre_deploy_cmd: rt::container_cmd() + " exec " + SERVICE + "-web npx prisma migrate deploy 2>/dev/null || true",
});

print("\n" + SERVICE + " " + VERSION + " is live.");
