# Energize (`nrg`)

[![CI](https://github.com/inou/nrgize-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/inou/nrgize-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A deployment toolkit written in Rust with a **Rhai** orchestration engine. You write
your deployment as a `.rhai` script; `nrg` evaluates it top-to-bottom, and the built-in
functions (`ssh_exec`, `http_get`, `state_set`, …) have real side effects as evaluation
reaches them. It's orchestration in a real scripting language — loops, conditionals,
functions, modules, `try`/`catch` — not YAML templating or a restricted config DSL.

The core supports arbitrary toolchains through SSH, file transfers, checked commands,
preflights and transactions. Optional recipes provide versioned artifact releases or
health-gated container rollouts, plus framework defaults for Rails, Django, Next.js,
Phoenix and Laravel. Run history, status checks, locks and encrypted secrets support
operating those workflows after deployment.

Interrupted or ambiguous cutovers retain recovery journals and may require manual reconciliation.
See the [September audit remediation](docs/audit-2026-09-05/REMEDIATION.md) for guarantees and remaining limits.

There are two ways to run a script, over **one** engine:

- `nrg exec [file]` — evaluate a `.rhai` module top-to-bottom (defaults to `Energize.rhai`).
- `nrg run <fn> [args...]` — load the same file, then **call a function** defined in it.

## General workflows and framework recipes

Use `nrg init --template release` for versioned artifact directories with explicit
activation and health hooks. Optional `std/release_recipes` helpers provide Rails,
Django, Next.js, Phoenix and Laravel build defaults. The core requires no specific
language or container runtime. See [workflow examples](docs/workflows.md) for
structured command options, release rollback, and durable run history.

Test those workflows with [recipe contracts and the failure playground](docs/rehearsal.md):
`nrg rehearse` validates declarations; `--execute --faults` exercises real commands
and recovery checks in a fresh local workspace. Includes a runnable HTTP example.

Use [private file transfers and preflights](docs/builtins.md#deployment-safety-apis)
for explicit destination permissions and capability checks. The
[app-scoped mise recipe](docs/stdlib.md#app-scoped-mise-stdmise--libmise) covers
Erlang/Elixir dependency ordering without changing global defaults.

## Quick Start

```bash
# Scaffold a starter Energize.rhai — or a framework-specific one:
# nrg init --template release   # generic artifact directories
# nrg init --template rails     # container starter; also django, nextjs, phoenix, laravel
nrg init

# List the functions defined in it (each is a `nrg run` entry point)
nrg tasks

# Call a function
nrg run deploy

# Or evaluate the whole file top-to-bottom
nrg exec

# Preview the deploy function without executing its mutating operations
nrg run deploy --dry-run

# Validate Rhai syntax; add --checks checks.json for declared capabilities
nrg doctor
```

An optional container `deploy()`, using the embedded standard library (`import "std/…"` — no
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

- **Generic directory releases** — `std/release` prepares a versioned directory, runs
  explicit build/migration hooks, switches `current`, and checks activation. Failed
  activation triggers best-effort restoration of the previous release. See [Workflows](docs/workflows.md).
- **Framework recipes** — optional build defaults for Rails, Django, Next.js, Phoenix
  and Laravel; override the commands and choose your own runtime and supervisor.
- **Health-gated container rollouts** — rolling updates with proxy switching and
  best-effort fleet rollback. Supports kamal-proxy and Caddy. Failures during recovery
  can require manual reconciliation; see [Container Deployment](docs/deploy.md).
- **Honest dry runs** — container/state simulation keeps planned operations internally
  consistent. Arbitrary shell operations are marked execution-unverified; use explicit
  preflight probes for runtime capabilities. See [Safety Features](docs/safety.md).
- **Checked execution and diagnostics** — named streaming steps with cwd/env/stdin,
  timeouts and explicit retry controls; durable start/finish events and redacted failures.
- **Container rollback** — `nrg rollback <service>` or `deploy::rollback(...)`, backed by a
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
- **Prebuilt binaries** — a `curl | sh` installer and Homebrew tap; see
  [Installation](#installation).

See [`docs/roadmap.md`](docs/roadmap.md) for the full feature-gap tracking this project
uses to prioritize what ships next.

## Commands

| Command | Description |
|---|---|
| `nrg rehearse [file]` | Validate recipe contracts; `--execute` runs them locally, `--faults` enables failure scenarios. |
| `nrg exec [file]` | Evaluate a `.rhai` module top-to-bottom. `--dry-run` to plan. |
| `nrg run <fn> [args...]` | Call a function defined in the orchestration file. `--file` / `--dry-run` / `--dest`. |
| `nrg tasks` | List the functions defined in the orchestration file. |
| `nrg init [--template <name>]` | Scaffold a generic script, directory release, or framework container starter. |
| `nrg doctor [--host h]...` | Validate syntax and explicitly declared deployment capabilities; `--checks FILE`. |
| `nrg status [service] [--check] [--json]` | Show container state; opt into automation exit checks and JSON. |
| `nrg logs <service>` | Tail a service's container logs across its deployed hosts. |
| `nrg app exec <service> [cmd...]` | Run a command (or an interactive console with `-i`) inside a service's live container. |
| `nrg setup --host h...` | Bootstrap a fresh host: install Docker if absent, create the network, boot the proxy. |
| `nrg audit [filter]` | Show failed step details; `--run ID`, `--incomplete`, and `--json` expose run history. |
| `nrg remove <service>` | Stop and remove a service's container from its deployed hosts. |
| `nrg rollback <service>` | Roll a container service back to a previous image. Directory releases use their Rhai rollback wrapper. |
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
| [Workflows and Recipes](docs/workflows.md) | Generic releases, framework defaults, execution options and run history |
| [Recipe Contracts and Rehearsal](docs/rehearsal.md) | First/repeat/restart contracts, real failure injection, disposable workspaces and reports |
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
`--version vX.Y.Z` or `$NRG_VERSION`). See `scripts/install.sh --help` for every flag.

**Homebrew** — this repository also serves as the tap, with its formula at
[`Formula/nrg.rb`](Formula/nrg.rb):

```bash
# Homebrew 6+ checks formula trust while adding the tap.
if brew help trust >/dev/null 2>&1; then brew trust --formula inou/nrg/nrg; fi
brew tap inou/nrg https://github.com/inou/nrgize-rs.git
brew install inou/nrg/nrg
```

Use the explicit repository URL when first adding the tap. To upgrade later, run
`brew update && brew upgrade inou/nrg/nrg`. The formula installs a prebuilt binary;
Rust is not required.

`brew tap inou/nrg` alone looks for the nonexistent `inou/homebrew-nrg`
repository. Use `upgrade`, not `update inou/nrg/nrg`, to update the installed
package. If `nrg --version` stays old, check `type -a nrg`: a prior installation
in `~/.local/bin` may shadow Homebrew. See the [migration instructions](docs/getting-started.md#homebrew-troubleshooting).
Homebrew installs the tagged release pinned in the formula, not unreleased `main`.

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
| `rsync`   | Scripts that explicitly use rsync             | Usually pre-installed                  |
| `scp`     | Scripts that explicitly use scp              | Part of OpenSSH                        |
| `docker`  | Container deployments                 | https://docs.docker.com/get-docker     |
| `podman`  | Container deployments (alternative)   | https://podman.io/getting-started      |
| OrbStack  | Container deployments (macOS)         | https://orbstack.dev                   |

First-class `upload_file` / `download_file` transfers use SSH and standard remote
POSIX tools; they do not require rsync or scp. `nrg doctor --checks checks.json`
checks declared capabilities. A fresh non-container deployment does not require
age or a container runtime. `--legacy-tools` opts into the old blanket checks.

## SSH config integration

`nrg` reads `~/.ssh/config` and resolves host aliases automatically — the same names your
orchestration scripts use work with `nrg ssh`.

## Contributing

See [`docs/architecture.md`](docs/architecture.md) for engine internals and
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the development/testing workflow.

## License

[MIT](LICENSE)
