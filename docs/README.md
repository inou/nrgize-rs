# Energize (`nrg`) — Documentation

`nrg` is a Rhai-scripted deployment toolkit (SSH orchestration, directory releases and optional
health-gated container rollouts): you write your
deployment as a `.rhai` script and the built-in functions have real side effects as evaluation
reaches them. The core supports any toolchain; optional recipes cover directory
releases, container rollouts and framework build defaults.

Start with the [project README](../README.md) for the overview, then dive in here.

## Guides

| Guide | What it covers |
|---|---|
| [**Getting Started**](getting-started.md) | Install, scaffold (`nrg init`), your first deploy, `nrg exec` vs `nrg run`, `--dry-run`. |
| [**CLI Reference**](cli.md) | Every command and flag: `rehearse`, `exec`, `run`, `tasks`, `init`, `doctor`, `status`, `logs`, `app exec`, `setup`, `audit`, `remove`, `rollback`, `lock`, `vendor`, `ssh`, `secrets`. |
| [**Builtins Reference**](builtins.md) | Every runtime builtin: exact signatures, return types, and dry-run behavior. |
| [**Standard Library**](stdlib.md) | The embedded `std/*` modules: `runtime`, `docker`, `proxy`, `caddy`, `healthcheck`, `registry`, `bunny`, `deploy`, `recipe`, `notify`, `mise`, `release`, `release_recipes`, `contracts`. |
| [**Fleet-Atomic Deploy**](deploy.md) | `deploy()` lifecycle, `rollback()`, accessories, state keys, lifecycle hooks, and the Caddy proxy. |
| [**Safety Features**](safety.md) | Dry-run, state locking, secrets, and transactions in depth, with the guarantees and limits. |
| [**Authoring Guide**](authoring.md) | Writing `Energize.rhai`: config maps, the Rhai gotchas, `Secret` handling, the failure contract. |
| [**Architecture**](architecture.md) | Engine internals (`RunCtx`, builtins, the sim, the transaction stack) for contributors. |
| [**Framework Examples**](examples.md) | Rails / Django / Next.js / Phoenix / Laravel walkthroughs + how to use one. |
| [Workflows and recipes](workflows.md) | Generic releases, framework defaults, checked command options, and run history. |
| [Recipe contracts and rehearsal](rehearsal.md) | First/repeat/restart contracts, failure injection, disposable workspaces, and reports (`nrg rehearse`). |
| [Deployment validation](deployment-validation.md) | Record of the 2026-09-14 local deployment-safety validation run. |
| [Product roadmap](roadmap.md) | What is still open. |

## Reading order

- **New here?** [Getting Started](getting-started.md) → [Authoring Guide](authoring.md) →
  [Workflows and recipes](workflows.md), then the container guide if needed.
- **Looking something up?** [CLI Reference](cli.md), [Builtins Reference](builtins.md),
  [Standard Library](stdlib.md), [Recipe contracts and rehearsal](rehearsal.md).
- **Want to trust it in production?** [Safety Features](safety.md).
- **Contributing?** [Architecture](architecture.md), then the [Roadmap](roadmap.md) for known gaps.
