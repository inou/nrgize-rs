// =============================================================================
// Energize.rhai — Laravel Deployment
// =============================================================================
//
// Zero-downtime Docker deployment for Laravel applications.
//
// What this does:
//   1. Builds your Laravel Docker image locally
//   2. Pushes to your container registry
//   3. Pulls on all web servers
//   4. Runs artisan migrate, config/route/view cache
//   5. Rolling deploy with kamal-proxy traffic switching
//   6. Health check on /up (Laravel 11+ default)
//
// Prerequisites:
//   - Container runtime installed locally and on all hosts
//     (Docker, Podman, or OrbStack — auto-detected by default)
//   - SSH access to all hosts (key-based auth)
//   - Container registry credentials
//   - A Dockerfile for your Laravel project
//
// Usage:
//   DEPLOY_TAG=v1.0.0 nrg exec
//   DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
//
// Required secrets:
//   REGISTRY_PASSWORD  — Container registry token
//   DATABASE_URL       — Full mysql/postgres connection string
//   APP_KEY            — Laravel APP_KEY (base64:...)
//
// Health check:
//   Laravel 11+ includes /up by default.
//   For older versions, add: Route::get('/up', fn() => response()->json(['status' => 'ok']));
//
// Dockerfile tips for Laravel:
//   - Use PHP 8.3+ with FPM or a Swoole/FrankenPHP/Octane server
//   - Install composer deps with --no-dev --optimize-autoloader
//   - Run `php artisan config:cache` in the build stage for faster boot
//   - Expose port 8000 (Octane) or 9000 (FPM behind nginx)
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

// Application settings — 8000 for Octane, 9000 for FPM
let APP_PORT = 8000;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

let APP_KEY      = reveal(secret("APP_KEY"));
let DATABASE_URL = reveal(secret("DATABASE_URL"));
let DB_HOSTNAME  = DB_HOST.split("@")[1];

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
deploy::accessory_run(DB_HOST, SERVICE + "-mysql", "mysql:8", #{
    ports:   #{ "3306": "3306" },
    envs:    #{
        "MYSQL_DATABASE":      SERVICE,
        "MYSQL_USER":          SERVICE,
        "MYSQL_PASSWORD":      reveal(secret("DB_PASSWORD")),
        "MYSQL_ROOT_PASSWORD": reveal(secret("DB_ROOT_PASSWORD")),
    },
    volumes: #{ "myapp-mysql-data": "/var/lib/mysql" },
});

deploy::accessory_run(DB_HOST, SERVICE + "-redis", "redis:7-alpine", #{
    ports:   #{ "6379": "6379" },
    volumes: #{ "myapp-redis-data": "/data" },
});

// 3. Deploy
//    Migrate + cache before switching traffic — joined with " && " via the `join` builtin.
let pre_deploy = join([
    rt::container_cmd() + " exec " + SERVICE + "-web php artisan migrate --force 2>/dev/null || true",
    rt::container_cmd() + " exec " + SERVICE + "-web php artisan config:cache 2>/dev/null || true",
    rt::container_cmd() + " exec " + SERVICE + "-web php artisan route:cache 2>/dev/null || true",
    rt::container_cmd() + " exec " + SERVICE + "-web php artisan view:cache 2>/dev/null || true",
], " && ");

deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: APP_PORT,
    envs: #{
        "APP_ENV":          "production",
        "APP_DEBUG":        "false",
        "APP_KEY":          APP_KEY,
        "APP_URL":          env_or("APP_URL", "https://example.com"),
        "LOG_CHANNEL":      "stderr",
        "DATABASE_URL":     DATABASE_URL,
        "REDIS_HOST":       DB_HOSTNAME,
        "REDIS_PORT":       "6379",
        "CACHE_STORE":      "redis",
        "SESSION_DRIVER":   "redis",
        "QUEUE_CONNECTION": "redis",
    },
    health_path:     "/up",
    health_attempts: 30,
    health_interval: 2,
    pre_deploy_cmd:  pre_deploy,
});

print("\n" + SERVICE + " " + VERSION + " is live.");

// ---------------------------------------------------------------------------
// Queue Worker (optional)
// ---------------------------------------------------------------------------
//
// To run a dedicated queue worker alongside your web containers, deploy the
// worker process directly with docker_run (workers don't need proxy routing):
//
//   import "lib/docker" as docker;
//   for host in WEB_HOSTS {
//       docker::docker_pull(host, FULL_IMAGE);
//       docker::docker_stop(host, SERVICE + "-worker");
//       docker::docker_remove(host, SERVICE + "-worker");
//       docker::docker_run(host, FULL_IMAGE, SERVICE + "-worker", #{
//           envs:  #{ /* ... same as above ... */ },
//           extra: "php artisan queue:work --sleep=3 --tries=3 --max-time=3600",
//       });
//   }
