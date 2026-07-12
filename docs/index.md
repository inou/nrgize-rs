---
title: Home
nav_order: 1
permalink: /
---

# Energize (`nrg`)
{: .fs-9 }

A Rust deployment toolkit with a **Rhai** orchestration engine — fleet-atomic,
zero-downtime Docker deploys with a real dry-run, locked state, tagged secrets,
and transactional rollback.
{: .fs-6 .fw-300 }

[Get started](getting-started.md){: .btn .btn-primary .fs-5 .mb-4 .mb-md-0 .mr-2 }
[View on GitHub](https://github.com/inou/nrgize-rs){: .btn .fs-5 .mb-4 .mb-md-0 }

---

## What is `nrg`?

You write your deployment as a `.rhai` script. `nrg` evaluates it top-to-bottom, and the
built-in functions — `ssh_exec`, `http_get`, `state_set`, … — have **real side effects** as
evaluation reaches them. It's orchestration in a real scripting language: loops, conditionals,
functions, modules, `try`/`catch` — not YAML templating, not a restricted config DSL.

The shipped standard library turns that into a **Kamal-style, fleet-atomic, zero-downtime**
Docker deploy with automatic rollback.

```bash
nrg init                 # scaffold an Energize.rhai
nrg run deploy           # call the deploy() function in it
nrg exec --dry-run       # preview every side effect, perform none
```

There's one engine and two ways in:

- **`nrg exec [file]`** — evaluate a `.rhai` module top-to-bottom.
- **`nrg run <fn> [args]`** — load the same file, then call a function in it.

## Why it's different

Most deploy scripts fail halfway and leave you guessing. `nrg`'s four safety features are the
point — see **[Safety Features](safety.md)**:

| | |
|---|---|
| **`--dry-run`** | Not just "skip the commands" — a container/state **simulation** so a deploy dry-run takes the same branches a real run would, and prints a plan. |
| **State locking** | Project-root–anchored, atomically written, corruption-fatal, advisory-locked, re-entrant. No CWD surprises, no torn writes, no silent resets. |
| **Secrets** | A tagged `Secret` type that can't be printed, concatenated, or persisted — only `reveal()`/`sh_quote()` expose it. Passwords reach `--password-stdin` **off-argv**. |
| **Transactions** | `transaction()` / `on_rollback()` — a deploy that throws on host 3 of 5 unwinds the **whole fleet** (proxy restored, new containers removed); the fleet is never left half-deployed. |

## Fleet-atomic deploy

`deploy()` wraps the entire rolling loop in **one transaction**. Each host's new container
starts on a fresh port; the old container is kept under its name until a single **post-commit**
cleanup. Any mid-fleet failure restores every touched host to the old version. The proxy is
pluggable — `cfg.proxy: "kamal"` (default) or `"caddy"`. See **[Fleet-Atomic Deploy](deploy.md)**.

```rhai
import "lib/deploy" as deploy;

fn ship(tag) {
    deploy::deploy(["web1", "web2"], "ghcr.io/org/app:" + tag, "app", #{
        container_port: 3000,
        health_path:    "/up",
        proxy:          "caddy",            // or "kamal" (default)
        domain:         "app.example.com",  // Caddy: automatic Let's Encrypt TLS
    });
}
```

```bash
nrg run ship v42 --dry-run   # preview the whole fleet-atomic plan
nrg run ship v42             # ship it
```

## Documentation

| Guide | What it covers |
|---|---|
| [Getting Started](getting-started.md) | Install, scaffold, your first deploy, `exec` vs `run`, `--dry-run`. |
| [CLI Reference](cli.md) | Every command and flag. |
| [Builtins Reference](builtins.md) | Every runtime builtin — signatures, return types, dry-run behavior. |
| [Standard Library](stdlib.md) | The `lib/*.rhai` modules. |
| [Fleet-Atomic Deploy](deploy.md) | `deploy()` lifecycle, rollback, accessories, proxy choice. |
| [Safety Features](safety.md) | Dry-run, state locking, secrets, transactions — in depth. |
| [Authoring Guide](authoring.md) | Writing `Energize.rhai`: Rhai idioms and gotchas. |
| [Architecture](architecture.md) | Engine internals for contributors. |
| [Framework Examples](examples.md) | Rails / Django / Next.js / Phoenix / Laravel walkthroughs. |

## Install

Prebuilt binaries for macOS (arm64/x86_64) and Linux (x86_64/arm64) are built by CI and
published to GitHub Releases whenever a `vX.Y.Z` tag is cut:

```bash
curl -fsSL https://raw.githubusercontent.com/inou/nrgize-rs/main/scripts/install.sh | sh
```

Or build from source (recent stable Rust):

```bash
git clone https://github.com/inou/nrgize-rs
cd nrgize-rs
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

Then `nrg doctor` to check your tools. Head to **[Getting Started](getting-started.md)**.
