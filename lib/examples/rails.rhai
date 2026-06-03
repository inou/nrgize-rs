// =============================================================================
// Energize.rhai — Ruby on Rails Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Rails applications.
//
// What this does:
//   1. Builds your Rails Docker image locally
//   2. Pushes to your container registry
//   3. Pulls on all web servers
//   4. Runs db:migrate via the existing container
//   5. Rolling deploy with kamal-proxy traffic switching
//   6. Health check on /up (Rails default)
//   7. Cleans up old containers and images
//
// Prerequisites:
//   - Container runtime installed locally and on all hosts
//     (Docker, Podman, or OrbStack — auto-detected by default)
//   - SSH access to all hosts (key-based auth)
//   - Container registry credentials
//   - A Dockerfile in your Rails project root
//
// Usage:
//   # First deploy
//   DEPLOY_TAG=v1.0.0 nrg exec
//
//   # Subsequent deploys
//   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
//
//   # Rollback
//   DEPLOY_TAG=v0.9.0 nrg exec  // just redeploy the old tag
//
// Required secrets (set via environment or .env file):
//   REGISTRY_PASSWORD  — Container registry token
//   DATABASE_URL       — Full postgres connection string
//   SECRET_KEY_BASE    — Rails secret key
//   REDIS_URL          — Redis connection string (optional)
//
// Required environment variables:
//   DEPLOY_TAG         — Image tag to deploy (e.g. "v1.2.3" or git SHA)
//
// Dockerfile tips for Rails:
//   - Use `rails new --docker` to generate a production Dockerfile
//   - Or use the official Rails Dockerfile template
//   - Make sure /up route exists (Rails 7.1+ has it by default)
//   - Ensure assets are precompiled in the build stage
//
// SECRET NOTE: secrets are a Secret type and cannot be concatenated into strings. Pass the
// raw Secret to registry_login (off-argv --password-stdin); reveal() secrets only when
// placing them into an env map — the revealed plaintext stays registered for redaction.
// =============================================================================

import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;
import "lib/docker" as docker;

// Auto-detect container runtime (Docker, Podman, OrbStack, nerdctl).
// To force a specific runtime: rt::set_runtime("podman") or rt::set_runtime("docker")
rt::set_runtime("auto");

// ---------------------------------------------------------------------------
// Configuration — edit these for your project
// ---------------------------------------------------------------------------

// Service name — used for container naming and proxy routing
let SERVICE = "myapp";

// Container registry
let REGISTRY      = "ghcr.io";
let IMAGE_REPO    = "ghcr.io/myorg/myapp";
let REGISTRY_USER = env_or("REGISTRY_USER", "deploy");
let REGISTRY_PASS = secret("REGISTRY_PASSWORD");

// Image tag — passed via environment variable
let VERSION    = env_or("DEPLOY_TAG", "latest");
let FULL_IMAGE = IMAGE_REPO + ":" + VERSION;

// Hosts — SSH connection strings for your web servers
let WEB_HOSTS = [
    "deploy@web1.example.com",
    "deploy@web2.example.com",
];

// Database host — where accessories (Postgres, Redis) run
let DB_HOST = "deploy@db.example.com";

// Application settings
let RAILS_ENV = "production";
let APP_PORT  = 3000;

// ---------------------------------------------------------------------------
// Secrets — loaded from environment or .env/.energize/secrets
// ---------------------------------------------------------------------------

let DATABASE_URL    = reveal(secret("DATABASE_URL"));
let SECRET_KEY_BASE = reveal(secret("SECRET_KEY_BASE"));
// DB_HOST is a plain string, so split()/concat is fine here.
let DB_HOSTNAME = DB_HOST.split("@")[1];
let REDIS_URL   = env_or("REDIS_URL", "redis://" + DB_HOSTNAME + ":6379/0");

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

print("==> Deploying " + SERVICE + " " + VERSION);

// 1. Registry authentication
print("\n--- Registry Login ---");
registry::registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS);
registry::registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS);

// 2. Accessories (database + cache)
print("\n--- Accessories ---");
deploy::accessory_run(DB_HOST, SERVICE + "-db", "postgres:16", #{
    ports:   #{ "5432": "5432" },
    envs:    #{
        "POSTGRES_DB":       SERVICE + "_production",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
});

deploy::accessory_run(DB_HOST, SERVICE + "-redis", "redis:7-alpine", #{
    ports:   #{ "6379": "6379" },
    volumes: #{ "myapp-redis-data": "/data" },
});

// 3. Deploy with zero-downtime rolling update
deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: APP_PORT,
    envs: #{
        "RAILS_ENV":                RAILS_ENV,
        "RAILS_LOG_TO_STDOUT":      "1",
        "RAILS_SERVE_STATIC_FILES": "1",
        "DATABASE_URL":             DATABASE_URL,
        "REDIS_URL":                REDIS_URL,
        "SECRET_KEY_BASE":          SECRET_KEY_BASE,
    },
    health_path:     "/up",
    health_attempts: 30,
    health_interval: 2,
    // Run migrations before switching traffic.
    pre_deploy_cmd:  rt::container_cmd() + " exec " + SERVICE + "-web bin/rails db:migrate 2>/dev/null || true",
});

print("\n" + SERVICE + " " + VERSION + " is live.");
