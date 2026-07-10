---
title: Framework Examples
nav_order: 10
---

# Framework Examples

Energize (`nrg`) ships a set of ready-to-edit deployment configs under
[`lib/examples/`](https://github.com/inou/nrgize-rs/tree/main/lib/examples). Each one is a complete `Energize.rhai`: it
imports the standard library, picks a container runtime, logs into your
registry, starts accessories (Postgres/MySQL/Redis), and runs a zero-downtime
rolling deploy behind `kamal-proxy`.

These are starting points, not magic. You copy one into your project, fill in
your hosts/registry/secrets, preview with `nrg exec --dry-run`, then ship with
`nrg exec`.

> kamal-proxy is the only proxy Energize manages. There is no nginx, no TLS
> module, no Caddy, no provisioning step. Energize assumes the container runtime
> and SSH access already exist on your hosts.

## The examples

All examples live in `lib/examples/` and follow the same shape. The differences
that matter per framework are the app port, the health path, and the
pre-deploy command (migrations / asset compilation).

| Framework | File | App port | Health path |
|-----------|------|----------|-------------|
| Ruby on Rails | `lib/examples/rails.rhai` | `3000` | `/up` |
| Django | `lib/examples/django.rhai` | `8000` | `/health/` |
| Next.js | `lib/examples/nextjs.rhai` | `3000` | `/api/health` |
| Phoenix (Elixir) | `lib/examples/phoenix.rhai` | `4000` | `/health` |
| Laravel | `lib/examples/laravel.rhai` | `8000` | `/up` |
| Generic (`Energize.rhai`) | `lib/examples/Energize.rhai` | `3000` | `/up` |

These are the only examples in the tree. There is no separate "setup" or
"full-setup" example.

---

## What each example does

Every example follows the same lifecycle. Walking the Rails one top-to-bottom:

### 1. Imports (top level only)

```rhai
import "lib/runtime" as rt;
import "lib/deploy" as deploy;
import "lib/registry" as registry;
import "lib/docker" as docker;
```

`import` statements must be at the **top level** of the file (Rhai does not
allow `import` inside a function or block). Imports resolve **relative to the
directory of the file being executed**: `import "lib/runtime"` loads
`<dir-of-Energize.rhai>/lib/runtime.rhai`. This is why you vendor `lib/`
alongside your `Energize.rhai` (see [How to use one](#how-to-use-one)).

Imports are **per-file** — a module you import does not inherit the caller's
imports, so `lib/deploy.rhai` imports `lib/docker`, `lib/proxy`,
`lib/healthcheck`, and `lib/runtime` itself.

### 2. Select the container runtime

```rhai
rt::set_runtime("auto");
```

`set_runtime(runtime)` accepts `"docker"`, `"podman"`, `"nerdctl"`, or
`"auto"`. Auto-detection probes the local machine in order: docker → podman →
nerdctl (and labels Docker as `orbstack` if that's what's running).

> Dry-run caveat: auto-detect calls `local_exec`, which is a mutating-class
> builtin. Under `--dry-run` it does **not** actually probe — it records the
> action and returns synthetic empty output, so the first branch (docker)
> always wins and `"auto"` resolves to `"docker"` in a plan. That's the safe
> default. If you run a non-Docker runtime and want a real probe, call
> `rt::set_runtime("podman")` (etc.) explicitly instead of `"auto"`.

The runtime choice is stored in the process-global state store, so every
`lib/` module that shells out to the container CLI reads the same value.

### 3. Configuration

Plain `let` bindings you edit for your project — service name, registry,
image repo, hosts, ports:

```rhai
let SERVICE       = "myapp";
let REGISTRY      = "ghcr.io";
let IMAGE_REPO    = "ghcr.io/myorg/myapp";
let REGISTRY_USER = env_or("REGISTRY_USER", "deploy");
let REGISTRY_PASS = secret("REGISTRY_PASSWORD");

let VERSION    = env_or("DEPLOY_TAG", "latest");
let FULL_IMAGE = IMAGE_REPO + ":" + VERSION;

let WEB_HOSTS = [
    "deploy@web1.example.com",
    "deploy@web2.example.com",
];
let DB_HOST   = "deploy@db.example.com";
let APP_PORT  = 3000;            // 8000 for Django/Laravel, 4000 for Phoenix
```

- `env_or("NAME", "default")` reads an environment variable with a fallback.
- `secret("NAME")` returns a `Secret` (see [Secrets](#secrets-and-the-secret-type)).
- The image tag comes from `DEPLOY_TAG` (`env_or("DEPLOY_TAG", "latest")`), so
  you deploy a specific build with `DEPLOY_TAG=v1.2.3 nrg exec`.

### 4. Registry login

```rhai
registry::registry_login("local", REGISTRY, REGISTRY_USER, REGISTRY_PASS);
registry::registry_login_all(WEB_HOSTS, REGISTRY, REGISTRY_USER, REGISTRY_PASS);
```

`registry_login(host, server, username, password)` logs into a registry —
pass `"local"` for the build machine. The **`password` is a raw `Secret`**:
the library reveals it only at the last moment and streams it to
`--password-stdin` off-argv, so the plaintext never lands on the command line
or in a dry-run plan. `registry_login_all(hosts, …)` does the same across the
fleet.

There's also `registry::ecr_login(host, #{ region: "us-east-1", account_id: "" })`
for AWS ECR, which runs `aws ecr get-login-password | … login` entirely in the
remote shell (no Rhai-side secret involved).

### 5. Accessories (databases, caches)

```rhai
deploy::accessory_run(DB_HOST, SERVICE + "-db", "postgres:16", #{
    ports:   #{ "5432": "5432" },
    envs:    #{
        "POSTGRES_DB":       SERVICE + "_production",
        "POSTGRES_USER":     SERVICE,
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
});
```

`accessory_run(host, name, image, cfg)` starts a long-lived container **only if
it isn't already running** (idempotent), so re-running a deploy won't restart
your database. Config keys: `ports`, `envs`, `volumes`, `network`, `cmd`. The
Laravel example uses `mysql:8` instead of Postgres; the others use
`postgres:16` + `redis:7-alpine` (Next.js makes the DB optional and guards it
behind `if DATABASE_URL != ""`).

### 6. The deploy call

```rhai
deploy::deploy(WEB_HOSTS, FULL_IMAGE, SERVICE, #{
    container_port: APP_PORT,
    envs: #{
        "RAILS_ENV":       "production",
        "DATABASE_URL":    DATABASE_URL,        // revealed secret
        "SECRET_KEY_BASE": SECRET_KEY_BASE,     // revealed secret
        // ...
    },
    health_path:     "/up",
    health_attempts: 30,
    health_interval: 2,
    pre_deploy_cmd:  rt::container_cmd() + " exec " + SERVICE + "-web bin/rails db:migrate 2>/dev/null || true",
});
```

`deploy(hosts, image, service, cfg)` is the fleet-atomic, zero-downtime rolling
deploy. It builds the image locally, pushes it, pulls on every host, ensures
`kamal-proxy` is up, then rolls each host inside a **single transaction**: start
a new container under a unique name, wait for HTTP health, then switch proxy
traffic. If any host fails mid-roll, the transaction unwinds — restoring each
already-switched host's proxy to its old target and removing the new
containers — so the fleet is never left half-deployed. After the whole fleet is
up, a post-commit pass retires the old containers and prunes.

Config keys (with defaults from `lib/deploy.rhai`):

| Key | Default | Meaning |
|-----|---------|---------|
| `container_port` | `3000` | Port the app listens on inside the container |
| `envs` | `#{}` | Container environment map |
| `volumes` | `#{}` | Volume mounts |
| `health_path` | `"/up"` | HTTP health path checked before traffic switch |
| `health_attempts` | `30` | Health-check attempts |
| `health_interval` | `2` | Seconds between attempts |
| `health_consecutive` | `1` | Consecutive passing checks required before the new container counts as healthy (robustness review R12) |
| `health_timeout` | `30` | Per-request HTTP timeout in seconds for each health check (robustness review R12) |
| `build_context` | `"."` | Docker build context |
| `dockerfile` | `"Dockerfile"` | Dockerfile path |
| `build_args` | `#{}` | `--build-arg` map |
| `platform` | `""` | A single target platform (e.g. `"linux/amd64"`) other than the build machine's own |
| `skip_build` | `false` | Skip the local build |
| `skip_push` | `false` | Skip the registry push |
| `network` | `""` | Container network |
| `pre_deploy_cmd` | `""` | Command run on each host before the traffic switch |
| `post_deploy_cmd` | `""` | Command run after the fleet commits |

The `pre_deploy_cmd` is where each framework runs migrations / asset steps via
the existing container:

- **Rails:** `bin/rails db:migrate`
- **Phoenix:** `bin/migrate` (the release migrate script)
- **Django:** `migrate --noinput` then `collectstatic --noinput`, joined with
  `&&` via the `join([...], " && ")` builtin
- **Laravel:** `artisan migrate --force` + `config:cache` + `route:cache` +
  `view:cache`, also joined with `join`
- **Next.js:** none by default (commented hint for `prisma migrate deploy`)

Each per-framework command is built from `rt::container_cmd()` so it uses the
runtime you selected, and ends with `2>/dev/null || true` so a non-fatal step
doesn't abort the deploy.

### 7. Done

```rhai
print("\n" + SERVICE + " " + VERSION + " is live.");
```

The Laravel example also includes a commented **queue worker** block at the
bottom: deploy a worker process directly with `docker::docker_run(...)` and an
`extra:` command (`php artisan queue:work ...`). Workers don't need proxy
routing, so they aren't part of the rolling deploy.

---

## How to use one

### Step 1 — Copy the example into your project

Copy the example for your framework to your project root as `Energize.rhai`:

```bash
cp /path/to/energize/lib/examples/rails.rhai ./Energize.rhai
```

The entry file must be named `Energize.rhai` (or `energize.rhai`) to be
discovered automatically. Otherwise pass it explicitly: `nrg exec deploy.rhai`.

### Step 2 — Vendor the `lib/` directory next to it

Because `import "lib/runtime"` resolves relative to the directory of the file
being executed, the standard library must sit beside your `Energize.rhai`:

```bash
cp -R /path/to/energize/lib ./lib
```

Your project ends up like this:

```
my-project/
├── Energize.rhai          # your copied + edited config
├── lib/
│   ├── runtime.rhai
│   ├── deploy.rhai
│   ├── docker.rhai
│   ├── proxy.rhai
│   ├── healthcheck.rhai
│   └── registry.rhai
├── Dockerfile
└── ...your app...
```

If `lib/` isn't alongside `Energize.rhai`, the `import` statements fail to
resolve and the script won't compile.

### Step 3 — Edit the configuration

Open `Energize.rhai` and change the `let` bindings near the top: `SERVICE`,
`REGISTRY` / `IMAGE_REPO`, `WEB_HOSTS`, `DB_HOST`, and `APP_PORT` if your app
listens on a non-default port. Make sure your `Dockerfile` exists and your app
actually serves the health path (e.g. Rails 7.1+ has `/up`; Django needs you to
add a `/health/` view).

### Step 4 — Provide the secrets

Each example calls `secret("NAME")`. A secret is resolved in this order:

1. The environment variable `NRG_SECRET_<NAME-UPPERCASED>`
2. A `KEY=VALUE` line in `.energize/secrets`
3. A `KEY=VALUE` line in `.env`

So `secret("REGISTRY_PASSWORD")` reads `NRG_SECRET_REGISTRY_PASSWORD` first:

```bash
export NRG_SECRET_REGISTRY_PASSWORD="ghp_xxxxxxxxxxxx"
export NRG_SECRET_DATABASE_URL="postgres://myapp:pw@db.example.com:5432/myapp_production"
export NRG_SECRET_SECRET_KEY_BASE="$(openssl rand -hex 64)"
export NRG_SECRET_DB_PASSWORD="a-strong-password"
```

Or drop them in `.energize/secrets` (gitignored):

```
REGISTRY_PASSWORD=ghp_xxxxxxxxxxxx
DATABASE_URL=postgres://myapp:pw@db.example.com:5432/myapp_production
SECRET_KEY_BASE=...
DB_PASSWORD=a-strong-password
```

Secrets must be **at least 6 characters** — `secret()` throws on anything
shorter (a too-short value can't be safely redacted from output). The required
secret names per framework are listed in the comment header of each example;
the common ones are `REGISTRY_PASSWORD`, `DATABASE_URL`, `DB_PASSWORD`, plus the
framework's app secret (`SECRET_KEY_BASE` for Rails/Phoenix, `DJANGO_SECRET_KEY`
for Django, `APP_KEY` for Laravel).

### Step 5 — Preview with `--dry-run`

```bash
DEPLOY_TAG=v1.0.0 nrg exec --dry-run
```

`nrg exec [file]` evaluates the file top-to-bottom. With `--dry-run` it shows
the plan of side effects **without executing** them — it takes no state lock and
writes no state. Under dry-run:

- **Mutating builtins** (`ssh_exec`, `local_exec`, `docker_run`, proxy switches,
  …) are recorded into the plan instead of running.
- **Reads** are answered from an in-memory simulation/overlay, so the deploy's
  port picking and health checks behave consistently (the health stub agrees
  with the simulated container).
- **HTTP** calls short-circuit, and **sleeps are skipped**, so a dry-run is fast.

Read the rendered plan and confirm the hosts, image tag, env vars (secrets show
as `***`), and the order of operations look right.

### Step 6 — Ship

```bash
DEPLOY_TAG=v1.0.0 nrg exec
```

A live run takes an advisory state lock (so two deploys don't race), runs the
real side effects, and persists deploy state (`<service>.version`,
`<service>.image`, per-host proxy targets). Subsequent deploys:

```bash
DEPLOY_TAG=$(git rev-parse --short HEAD) nrg exec
```

To roll back, you can simply re-deploy a previous tag
(`DEPLOY_TAG=v0.9.0 nrg exec`), or call the library's `deploy::rollback(hosts,
service)` which redeploys the snapshotted previous image (skipping build and
push).

---

## Secrets and the `Secret` type

`secret("NAME")` returns a `Secret`, which is deliberately **not** a string. You
cannot concatenate a `Secret` into another string — that throws. There are two
correct ways to use one:

- **Pass the raw `Secret`** to a function that knows how to stream it safely off
  the command line — this is what `registry_login(..., REGISTRY_PASS)` does
  (delivered to `--password-stdin`). The plaintext never appears on argv or in
  the plan.
- **`reveal(secret("NAME"))`** to get the plaintext `String`, used only when you
  place it into an `envs` map. The revealed value stays registered for
  redaction, so it's masked as `***` in traces and dry-run output.

When you need a secret inside a shell command string, use `sh_quote(...)` rather
than building the command by hand.

A small Rhai gotcha visible in the examples: Rhai has **no string truthiness**,
so optional values are tested explicitly with `!= ""` (see how the Next.js
example guards its optional `DATABASE_URL` and `NEXT_PUBLIC_URL`). Numbers going
into an env map are converted with `.to_string()` (e.g. `APP_PORT.to_string()`).

---

## Per-framework quick reference

| Framework | Port | Health path | App secret | Pre-deploy |
|-----------|------|-------------|------------|------------|
| Rails | `3000` | `/up` | `SECRET_KEY_BASE` | `bin/rails db:migrate` |
| Django | `8000` | `/health/` | `DJANGO_SECRET_KEY` | `migrate` + `collectstatic` |
| Next.js | `3000` | `/api/health` | — (DB optional) | none (Prisma optional) |
| Phoenix | `4000` | `/health` | `SECRET_KEY_BASE` | `bin/migrate` |
| Laravel | `8000` | `/up` | `APP_KEY` | `migrate` + cache warmup |

Every framework also needs `REGISTRY_PASSWORD` for registry login and, where it
runs a database accessory, `DB_PASSWORD` (Laravel additionally uses
`DB_ROOT_PASSWORD`). Check the comment header at the top of each example file for
its exact required-secret list and any framework-specific Dockerfile tips.
