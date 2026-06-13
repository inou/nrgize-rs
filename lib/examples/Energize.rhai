// Energize.rhai — Example deployment configuration for a Rails/Phoenix/Node app.
//
// This is the Kamal-equivalent: a single file that defines your entire
// deployment workflow. Run with: nrg exec
//
// Customize the variables below, add your hosts, and you have a production
// deployment pipeline.
//
// PORT NOTE (Starlark -> Rhai):
//   - `load("lib/x.star", "fn")` becomes `import "lib/x" as x;` + `x::fn(...)`.
//   - Keyword args become a single config map: deploy(hosts, image, service, #{...}).
//   - Secrets are a Secret type: they CANNOT be concatenated into a string. Put a
//     reveal(secret("X")) into an env map (revealed plaintext, still redacted in traces),
//     but pass the raw Secret to registry_login (it streams it to --password-stdin off-argv).

import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;
import "lib/docker" as docker;

// Auto-detect container runtime (Docker, Podman, OrbStack, nerdctl).
rt::set_runtime("auto");

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

let SERVICE    = "myapp";
let IMAGE      = "ghcr.io/myorg/myapp";
let VERSION    = env_or("DEPLOY_TAG", "latest");
let FULL_IMAGE = IMAGE + ":" + VERSION;

let WEB_HOSTS = [
    "deploy@10.0.0.1",
    "deploy@10.0.0.2",
];

let DB_HOST = "deploy@10.0.0.3";

let REGISTRY_SERVER   = "ghcr.io";
let REGISTRY_USER     = env_or("REGISTRY_USER", "deploy");
let REGISTRY_PASSWORD = secret("REGISTRY_PASSWORD");

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

// 1. Authenticate with the registry everywhere (the Secret streams to --password-stdin).
print("==> Registry login");
registry::registry_login("local", REGISTRY_SERVER, REGISTRY_USER, REGISTRY_PASSWORD);
registry::registry_login_all(WEB_HOSTS, REGISTRY_SERVER, REGISTRY_USER, REGISTRY_PASSWORD);

// 2. Ensure accessories are running.
print("\n==> Accessories");
deploy::accessory_run(DB_HOST, SERVICE + "-db", "postgres:16", #{
    ports:   #{ "5432": "5432" },
    envs:    #{
        "POSTGRES_DB":       SERVICE + "_production",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{},
});

deploy::accessory_run(DB_HOST, SERVICE + "-redis", "redis:7", #{
    ports:   #{ "6379": "6379" },
    volumes: #{},
});

// 3. Deploy web service with zero-downtime rolling update.
//    DB_HOST here is "deploy@10.0.0.3" — take the hostname part for the URLs. The DB password is
//    a Secret; reveal() it AND url_encode() it so a password containing @ / : / # can't corrupt
//    the postgres:// connection string (the revealed plaintext stays registered for redaction).
let DB_PASSWORD = url_encode(reveal(secret("DB_PASSWORD")));
let DB_HOSTNAME = DB_HOST.split("@")[1];
deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: 3000,
    envs: #{
        "DATABASE_URL":     "postgres://" + SERVICE + ":" + DB_PASSWORD + "@" + DB_HOSTNAME + ":5432/" + SERVICE + "_production",
        "REDIS_URL":        "redis://" + DB_HOSTNAME + ":6379/0",
        "RAILS_ENV":        "production",
        "SECRET_KEY_BASE":  reveal(secret("SECRET_KEY_BASE")),
    },
    health_path: "/up",
    // Migrations run ONCE in a throwaway container built on the NEW image (not an exec into the
    // old one), before any traffic switches; a failure aborts and unwinds the fleet.
    pre_deploy: "bin/rails db:migrate",
});

print("\nDone! " + SERVICE + " " + VERSION + " is live.");
