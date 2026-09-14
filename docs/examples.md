---
title: Framework Examples
nav_order: 10
---

# Framework Examples

Framework recipes are optional. The same nrg core handles SSH, private file
transfers, checked commands, preflight checks and transactions for any toolchain.
Choose the deployment lifecycle first, then add framework defaults where useful.

| Workflow | Starting point | App-specific choices |
| --- | --- | --- |
| Custom orchestration | `nrg init` | Commands, transfers, checks and rollback actions |
| Versioned artifact directories | `nrg init --template release` | Prepare/build/migrate, supervisor and health commands |
| Framework directory release | `std/release` plus `std/release_recipes` | Same hooks, with overridable build defaults |
| Framework container rollout | `nrg init --template rails` (or another framework below) | Image, hosts, registry, proxy, environment and accessories |

Container rollouts support kamal-proxy or Caddy. Directory releases do not require
containers or either proxy. Runtime installation and database services are explicit
choices; [app-scoped mise](stdlib.md#app-scoped-mise-stdmise--libmise) is one optional
provisioning helper. See [workflows](workflows.md) for lifecycle and recovery limits.

## Directory recipes

`std/release_recipes` exports `rails(cfg)`, `django(cfg)`, `nextjs(cfg)`,
`phoenix(cfg)` and `laravel(cfg)`. Each returns a config map; it performs no work
until you pass the map to `release::deploy`.

```rhai
import "std/release" as release;
import "std/release_recipes" as recipes;

fn config() {
    recipes::django(#{
        prepare: "cp -R /srv/staged/api/. .",
        env: #{ DJANGO_SETTINGS_MODULE: "api.settings.production" },
        migrate: ".venv/bin/python manage.py migrate --noinput",
        activate: "sudo -n systemctl restart api",
        health: "curl --fail --silent --show-error http://127.0.0.1:8000/health/",
        deactivate: "sudo -n systemctl stop api",
        timeout_secs: 300
    })
}
fn deploy(version) {
    release::deploy("deploy@web1", "/srv/api", version, config());
}
fn rollback(version) {
    release::rollback("deploy@web1", "/srv/api", version, config());
}
```

```bash
nrg run deploy v42 --dry-run
# Run live after adapting the hooks and checking the intended host:
nrg run deploy v42
nrg audit --verbose
```

The Django default creates a per-release `.venv`, installs `requirements.txt`, and
runs `collectstatic`. Configure the service to execute the application from
`/srv/api/current/.venv` using your chosen server. The helper does not install or
configure that service. Override `build` for a different dependency manager or a
prebuilt artifact. An empty `build` skips the build hook.

The caller's `env` map merges into framework defaults; other keys replace defaults.
All recipes require caller-provided prepare, activate and health commands. Migration
is opt-in, and a release rollback does not reverse database changes. For Phoenix,
the [workflow guide](workflows.md#framework-defaults-you-can-replace) shows how to
combine the recipe with explicit app-scoped Erlang/Elixir provisioning.

## The container examples

The container starters are complete `Energize.rhai` files backed by
`recipe::standard_deploy`. They authenticate to a registry, optionally start
accessories, and perform a health-gated rolling deployment. Review the generated
configuration before running: these starters can create database/cache containers
and execute migrations.

| Framework | Source | App port | Example health path |
| --- | --- | --- | --- |
| Rails | [rails.rhai](https://github.com/inou/nrgize-rs/blob/main/lib/examples/rails.rhai) | 3000 | `/up` |
| Django | [django.rhai](https://github.com/inou/nrgize-rs/blob/main/lib/examples/django.rhai) | 8000 | `/health/` |
| Next.js | [nextjs.rhai](https://github.com/inou/nrgize-rs/blob/main/lib/examples/nextjs.rhai) | 3000 | `/api/health` |
| Phoenix | [phoenix.rhai](https://github.com/inou/nrgize-rs/blob/main/lib/examples/phoenix.rhai) | 4000 | `/health` |
| Laravel | [laravel.rhai](https://github.com/inou/nrgize-rs/blob/main/lib/examples/laravel.rhai) | 8000 | `/up` |

These paths are sample configuration; the application must implement the selected
endpoint. The framework label does not establish that an application's build,
release migration or health command exists.

## How to use one

### Scaffold without vendoring

```bash
nrg init --template rails
# Also supported: django, nextjs, phoenix, laravel; use release for artifact directories.
```

This writes a container starter using `import "std/recipe" as recipe;`. No copied
`lib/` directory is required. `nrg init` refuses to overwrite an existing file.

1. Set the service, immutable image tag, registry and deployment hosts.
2. Review environment values, volumes, published ports and accessories.
3. Adapt the health path and migration/release task to the app.
4. Supply the secrets requested by the generated file.
5. Run `nrg doctor` and explicit deployment capability checks.
6. Preview with `nrg exec --dry-run`, then use `nrg exec` for a live container rollout.

The framework container starters execute at the top level, so use **`nrg exec`**.
The directory starter defines functions, so use **`nrg run deploy VERSION`**.
A dry run proves neither runtime compatibility nor health; shell steps are marked
execution-unverified, and temporary-write preflights require explicit opt-in.

### Customize an embedded module

Run `nrg vendor` to materialize the embedded modules under `lib/`, then change the
relevant import from `std/X` to `lib/X`. `std/X` always selects the bundled module;
`lib/X` selects the project file. Imports belong at file top level and are resolved
per file. See the [authoring guide](authoring.md).

## Secrets and the `Secret` type

Resolve `secret("NAME")` before executing a command so its value is registered for
redaction. Prefer structured environment/stdin options and first-class transfers.
Do not interpolate a secret into a shell command.

```rhai
ssh_step("deploy@web1", "Read deployment credential", "./check-credential", #{
    cwd: "/srv/api",
    stdin: secret("DEPLOY_TOKEN").reveal(),
    timeout_secs: 30
});
upload_file("deploy@web1", "./runtime.env", "/srv/api/shared/runtime.env", "0600");
```

The upload replaces new or existing destination files with the requested permissions.
The parent directory must exist and be trusted. File contents stay out of argv,
plans and diagnostics. Environment/stdin values are delivered off nrg's argv and
registered for streaming redaction; commands must avoid exposing them through their
own arguments. See [builtins](builtins.md) and [safety](safety.md).

## Per-framework quick reference

| Framework | Directory build default | Optional migration hook |
| --- | --- | --- |
| Rails | Bundle install and asset precompilation | `bundle exec rails db:migrate` |
| Django | `.venv`, requirements install, collectstatic | `.venv/bin/python manage.py migrate --noinput` |
| Next.js | `npm ci --include=dev` and `npm run build` | App-specific |
| Phoenix | Production dependencies, compile, assets.deploy, release | Application-provided release task |
| Laravel | Composer install and config/route/view caches | `php artisan migrate --force` |

The defaults assume conventional project tasks and already-available build tools.
Tests exercise these helpers as Rhai configuration and orchestration. They do not
constitute live framework deployment validation; see the [validation record](deployment-validation.md).
