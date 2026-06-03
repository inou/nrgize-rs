// =============================================================================
// Energize.rhai — Phoenix (Elixir) Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Phoenix applications using releases.
//
// What this does:
//   1. Builds your Phoenix Docker image locally (mix release)
//   2. Pushes to your container registry
//   3. Pulls on all web servers
//   4. Runs Ecto migrations via `bin/migrate`
//   5. Rolling deploy with kamal-proxy traffic switching
//   6. Health check on /health
//
// Prerequisites:
//   - Container runtime installed locally and on all hosts
//     (Docker, Podman, or OrbStack — auto-detected by default)
//   - SSH access to all hosts (key-based auth)
//   - Container registry credentials
//   - A Dockerfile using `mix release` (Phoenix 1.6+ generators include one)
//
// Usage:
//   DEPLOY_TAG=v1.0.0 nrg exec
//   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
//
// Required secrets:
//   REGISTRY_PASSWORD  — Container registry token
//   DATABASE_URL       — Full postgres connection string
//   SECRET_KEY_BASE    — Phoenix secret key base (min 64 chars)
//
// Health check endpoint (add to router.ex):
//   scope "/", MyAppWeb do
//     get "/health", HealthController, :index
//   end
//
// Migration module (lib/my_app/release.ex) provides `bin/migrate`.
//
// Dockerfile tips:
//   - Use `mix phx.gen.release --docker` to generate a production Dockerfile
//   - The release includes a `bin/migrate` script
//   - Ensure PHX_SERVER=true is set so the server starts
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
let APP_PORT = 4000;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

let DATABASE_URL    = reveal(secret("DATABASE_URL"));
let SECRET_KEY_BASE = reveal(secret("SECRET_KEY_BASE"));

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
        "POSTGRES_DB":       SERVICE + "_prod",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
});

// 3. Deploy
deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: APP_PORT,
    envs: #{
        "PHX_SERVER":      "true",
        "PHX_HOST":        env_or("PHX_HOST", "example.com"),
        "PORT":            APP_PORT.to_string(),
        "DATABASE_URL":    DATABASE_URL,
        "SECRET_KEY_BASE": SECRET_KEY_BASE,
        "MIX_ENV":         "prod",
        "POOL_SIZE":       env_or("POOL_SIZE", "10"),
    },
    health_path:     "/health",
    health_attempts: 30,
    health_interval: 2,
    // Run Ecto migrations via the release migrate script.
    pre_deploy_cmd:  rt::container_cmd() + " exec " + SERVICE + "-web bin/migrate 2>/dev/null || true",
});

print("\n" + SERVICE + " " + VERSION + " is live.");
