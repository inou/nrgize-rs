# Energize (`nrg`) — Documentation

`nrg` is a Rust deployment toolkit with a **Rhai** orchestration engine: you write your
deployment as a `.rhai` script and the built-in functions have real side effects as evaluation
reaches them. The shipped standard library turns that into a Kamal-style, **fleet-atomic,
zero-downtime** Docker deploy with automatic rollback.

Start with the [project README](../README.md) for the overview, then dive in here.

## Guides

| Guide | What it covers |
|---|---|
| [**Getting Started**](getting-started.md) | Install, scaffold (`nrg init`), your first deploy, `nrg exec` vs `nrg run`, `--dry-run`. |
| [**CLI Reference**](cli.md) | Every command + flag: `exec`, `run`, `tasks`, `init`, `doctor`, `ssh`, `secrets`. |
| [**Builtins Reference**](builtins.md) | Every runtime builtin — exact signatures, return types, and dry-run behavior. |
| [**Standard Library**](stdlib.md) | The `lib/*.rhai` modules: `runtime`, `docker`, `proxy`, `healthcheck`, `registry`, `bunny`. |
| [**Fleet-Atomic Deploy**](deploy.md) | `deploy()` lifecycle, `rollback()`, accessories, state keys, and why kamal-proxy. |
| [**Safety Features**](safety.md) | Dry-run, state locking, secrets, and transactions — in depth, with the guarantees *and* limits. |
| [**Authoring Guide**](authoring.md) | Writing `Energize.rhai`: config maps, the Rhai gotchas, `Secret` handling, the failure contract. |
| [**Architecture**](architecture.md) | Engine internals (`RunCtx`, builtins, the sim, the transaction stack) for contributors. |
| [**Framework Examples**](examples.md) | Rails / Django / Next.js / Phoenix / Laravel walkthroughs + how to use one. |

## Reading order

- **New here?** [Getting Started](getting-started.md) → [Authoring Guide](authoring.md) →
  [Fleet-Atomic Deploy](deploy.md).
- **Looking something up?** [CLI Reference](cli.md), [Builtins Reference](builtins.md),
  [Standard Library](stdlib.md).
- **Want to trust it in production?** [Safety Features](safety.md).
- **Contributing?** [Architecture](architecture.md).

## Design history

The full design spec, per-phase implementation plans, and adversarial-review outcomes from the
Starlark→Rhai migration are archived under [`superpowers/`](superpowers/); older pre-migration
planning notes are in [`archive/`](archive/).
