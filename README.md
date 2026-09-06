# Energize (`nrg`)

[![CI](https://github.com/inou/nrgize-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/inou/nrgize-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A deployment toolkit written in Rust with a **Rhai** orchestration engine. You write
your deployment as a `.rhai` script; `nrg` evaluates it top-to-bottom, and the built-in
functions (`ssh_exec`, `http_get`, `state_set`, …) have real side effects as evaluation
reaches them. It's orchestration in a real scripting language — loops, conditionals,
functions, modules, `try`/`catch` — not YAML templating or a restricted config DSL.

The shipped standard library turns that into a Kamal-style, **health-gated rolling** Docker deploy with best-effort fleet rollback, plus the day-2 operations a real
team needs: logs, status, a distributed lock, an audit trail, multi-environment
destinations, encrypted secrets, and more (see [Features](#features) below).

Interrupted or ambiguous cutovers retain recovery journals and may require manual reconciliation.
See the [September audit remediation](docs/audit-2026-09-05/REMEDIATION.md) for guarantees and remaining limits.

There are two ways to run a script, over **one** engine:

- `nrg exec [file]` — evaluate a `.rhai` module top-to-bottom (defaults to `Energize.rhai`).
- `nrg run <fn> [args...]` — load the same file, then **call a function** defined in it.

## Quick Start

```bash
# Scaffold a starter Energize.rhai — or a framework-specific one:
# nrg init --template rails|django|nextjs|phoenix|laravel
nrg init

# List the functions defined in it (each is a `nrg run` entry point)
nrg tasks

# Call a function
nrg run deploy

# Or evaluate the whole file top-to-bottom
nrg exec

# Preview the side effects without performing any of them
nrg exec --dry-run

# Validate the file compiles and required tools are installed
nrg doctor
```

A minimal `deploy()`, using the embedded standard library (`import "std/…"` — no
vendoring needed):

```rhai
import "std/deploy" as deploy;

fn deploy() {
    deploy::deploy(["web1", "web2"], "ghcr.io/org/app:" + env_or("TAG", "latest"), "app", #{
        container_port: 3000,
        health_path:    "/up",
    });
}
```

```bash
nrg run deploy --dry-run   # preview the rolling deployment plan
nrg run deploy             # ship it
```

## Features

- **Fleet-atomic, zero-downtime deploys** — `deploy()` wraps the whole rolling rollout in
  one transaction; a mid-fleet failure unwinds every already-switched host back to the old
  version. Proxy-pluggable (`kamal-proxy` or Caddy with automatic Let's Encrypt TLS). See
  [Fleet-Atomic Deploy](docs/deploy.md).
- **A real dry-run** — `--dry-run` isn't "skip the commands"; it's a container/state
  **simulation**, so the plan takes the same branches a live run would. See
  [Safety Features](docs/safety.md).
- **Automatic rollback** — `nrg rollback <service>` or `deploy::rollback(...)`, backed by a
  snapshotted previous image; refuses to roll back to a mutable `:latest` tag it snapshotted
  automatically.
- **Day-2 operations** — `nrg status`, `nrg logs`, `nrg app exec` (console into a live
  container), `nrg audit` (redacted operational history), `nrg remove`.
- **Distributed deploy lock** — a cross-machine lock so two concurrent deploys/rollbacks of
  the same service can't corrupt state or double-book a port; `nrg lock status|acquire|release`
  for manual control (e.g. blocking deploys during a maintenance window).
- **Multi-environment destinations** — `--dest staging` namespaces state and secrets per
  environment from the same orchestration file, without clobbering another destination's
  deploy history.
- **Encrypted secrets** — a tagged `Secret` type that can't be printed, concatenated, or
  persisted in plaintext; `nrg secrets` wraps [age](https://github.com/FiloSottile/age)
  encryption for secrets committed to a repo.
- **Accessory lifecycle** — `accessory_stop`/`accessory_restart`/`accessory_upgrade` for
  long-lived containers (Postgres, Redis, …), on top of `accessory_run`.
- **Maintenance mode** — `proxy_maintenance(...)` for a suspend/resume or custom maintenance
  page, on both proxy backends.
- **Lifecycle hooks + notifications** — optional `hook_pre_deploy`/`hook_post_deploy`/
  `hook_post_rollback` functions, plus a `notify::slack`/`notify::webhook` stdlib helper.
- **Zero-vendoring embedded stdlib** — `import "std/deploy"` etc. work out of the box,
  version-locked to the binary; `nrg vendor` materializes it onto disk only if you want to
  customize a module.
- **Framework templates** — `nrg init --template rails|django|nextjs|phoenix|laravel`
  scaffolds a complete, production-shaped `Energize.rhai` for that stack.
- **Prebuilt binaries** — a `curl | sh` installer and (upcoming) Homebrew tap; see
  [Installation](#installation).

See [`docs/roadmap.md`](docs/roadmap.md) for the full feature-gap tracking this project
uses to prioritize what ships next.

## Commands

| Command | Description |
|---|---|
| `nrg exec [file]` | Evaluate a `.rhai` module top-to-bottom. `--dry-run` to plan. |
| `nrg run <fn> [args...]` | Call a function defined in the orchestration file. `--file` / `--dry-run` / `--dest`. |
| `nrg tasks` | List the functions defined in the orchestration file. |
| `nrg init [--template <framework>]` | Scaffold a starter `Energize.rhai`, or a framework-specific one. |
| `nrg doctor [--host h]...` | Check the file compiles, required tools are installed, and hosts are reachable. |
| `nrg status [service]` | Show the deployed version/image and per-host container state. |
| `nrg logs <service>` | Tail a service's container logs across its deployed hosts. |
| `nrg app exec <service> [cmd...]` | Run a command (or an interactive console with `-i`) inside a service's live container. |
| `nrg setup --host h...` | Bootstrap a fresh host: install Docker if absent, create the network, boot the proxy. |
| `nrg audit [filter]` | Show the redacted history of past `nrg exec`/`nrg run` invocations. |
| `nrg remove <service>` | Stop and remove a service's container from its deployed hosts. |
| `nrg rollback <service>` | Roll a service back to a previous image — no project-authored wiring needed. |
| `nrg lock <status\|acquire\|release> <service>` | Manually inspect/acquire/release a service's cross-machine deploy lock. |
| `nrg vendor [--force]` | Materialize the embedded stdlib onto disk as `lib/*.rhai`, for customization. |
| `nrg ssh <host>` | Open an interactive SSH session, resolving `~/.ssh/config` aliases. |
| `nrg secrets <cmd>` | Manage encrypted secrets (`init`/`encrypt`/`decrypt`/`seal`/`unseal`). |

Every command has `--help`; see [CLI Reference](docs/cli.md) for the full flag reference.
Set `NRG_TRACE=1` to trace each builtin invocation to stderr (with secrets redacted).

## Documentation

This README is the overview. The full reference lives in [`docs/`](docs/):

| Guide | What it covers |
|---|---|
| [Getting Started](docs/getting-started.md) | Install, scaffold, your first deploy, `exec` vs `run`, `--dry-run` |
| [CLI Reference](docs/cli.md) | Every command and flag |
| [Builtins Reference](docs/builtins.md) | Every runtime builtin — signatures, return types, dry-run behavior |
| [Standard Library](docs/stdlib.md) | The `lib/*.rhai` modules |
| [Fleet-Atomic Deploy](docs/deploy.md) | `deploy()` lifecycle, rollback, accessories, lifecycle hooks, and the proxy choice |
| [Safety Features](docs/safety.md) | Dry-run, state locking, secrets, and transactions in depth |
| [Authoring Guide](docs/authoring.md) | Writing `Energize.rhai`: Rhai idioms and gotchas |
| [Architecture](docs/architecture.md) | Engine internals for contributors |
| [Framework Examples](docs/examples.md) | Rails / Django / Next.js / Phoenix / Laravel walkthroughs |
| [Roadmap](docs/roadmap.md) | Feature-gap tracking: what's shipped, what's next |

## Installation

**Prebuilt binaries** — macOS (arm64/x86_64) and Linux (x86_64/arm64), published on
[GitHub Releases](https://github.com/inou/nrgize-rs/releases) whenever a `vX.Y.Z` tag is
cut (see `.github/workflows/release.yml`):

```bash
curl -fsSL https://raw.githubusercontent.com/inou/nrgize-rs/main/scripts/install.sh | sh
```

Downloads the right binary for your OS/arch, verifies its sha256 checksum, and installs it to
`~/.local/bin` (override with `--bin-dir DIR` or `$NRG_INSTALL_DIR`; pin a version with
`--version vX.Y.Z` or `$NRG_VERSION`). See `scripts/install.sh --help` for every flag. **No tag
has been cut yet**, so there's nothing to download until the first one is — build from source
until then.

**Homebrew** — a formula template lives at [`homebrew/nrg.rb`](homebrew/nrg.rb); once a real
tap exists (`inou/homebrew-nrg`), `brew tap inou/nrg && brew install nrg` will work the same
way.

**`cargo install nrg`** — planned as a fallback once the crate is published to crates.io; not
yet done (see [`docs/roadmap.md`](docs/roadmap.md) 3.1).

**From source** (requires a recent stable Rust):

```bash
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

### Optional Dependencies

| Tool      | Required for                          | Install                                |
|-----------|----------------------------------------|-----------------------------------------|
| `ssh`     | Remote execution                      | Part of OpenSSH (usually pre-installed) |
| `age`     | Secret encryption (`nrg secrets`)     | `brew install age` / `apt install age` |
| `rsync`   | File transfer (preferred)             | Usually pre-installed                  |
| `scp`     | File transfer (fallback)              | Part of OpenSSH                        |
| `docker`  | Container deployments                 | https://docs.docker.com/get-docker     |
| `podman`  | Container deployments (alternative)   | https://podman.io/getting-started      |
| OrbStack  | Container deployments (macOS)         | https://orbstack.dev                   |

`nrg doctor` checks for `age` and `ssh`, plus at least one of `rsync`/`scp` and one of
`docker`/`podman`.

## SSH config integration

`nrg` reads `~/.ssh/config` and resolves host aliases automatically — the same names your
orchestration scripts use work with `nrg ssh`.

## Contributing

See [`docs/architecture.md`](docs/architecture.md) for engine internals and
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the development/testing workflow.

## License

[MIT](LICENSE)
