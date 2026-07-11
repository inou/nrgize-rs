---
title: Getting Started
nav_order: 2
---

# Getting Started with Energize (`nrg`)

Energize is a deployment toolkit written in Rust with a [Rhai](https://rhai.rs)
orchestration engine. You write your deployment as a `.rhai` script; `nrg` evaluates it
top-to-bottom, and the built-in functions (`ssh_exec`, `http_get`, `state_set`, …) have
**real side effects** as evaluation reaches them. The shipped standard library turns that
model into a Kamal-style, fleet-atomic, zero-downtime Docker deploy with automatic rollback.

There is one engine, two ways to drive it:

- **`nrg exec [file]`** evaluates a module top-to-bottom (defaults to `Energize.rhai`).
- **`nrg run <fn> [args...]`** loads the same file, then **calls a function** defined in it.

This guide takes you from a clean machine to a first end-to-end deploy. For the full
reference, see the linked pages at the [bottom](#where-to-go-next).

---

## Install / build from source

There are no prebuilt binaries; build it with Cargo. You need a **recent stable Rust**
toolchain (install via [rustup](https://rustup.rs)).

```bash
git clone <repo-url> nrgize-rs
cd nrgize-rs
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

Confirm it runs:

```bash
nrg --help
```

### Optional external tools

`nrg` shells out to a few standard CLIs. Install only the ones your deploy actually uses.

| Tool      | Needed for                                   | Install                                       |
|-----------|----------------------------------------------|-----------------------------------------------|
| `ssh`     | Remote execution (`ssh_exec`, `nrg ssh`)     | Part of OpenSSH (usually pre-installed)        |
| `age`     | Encrypted secret management (`nrg secrets`)  | `brew install age` / `apt install age`         |
| `rsync`   | File transfer (preferred)                    | Usually pre-installed                          |
| `scp`     | File transfer (fallback)                     | Part of OpenSSH                                |
| `docker`  | Container deployments                        | <https://docs.docker.com/get-docker>           |
| `podman`  | Container deployments (alternative)          | <https://podman.io/getting-started>            |

On macOS, [OrbStack](https://orbstack.dev) works as a Docker variant and is auto-detected.

### `nrg doctor`

`nrg doctor` checks that your orchestration file **compiles** and that the tools the stdlib
relies on are on `PATH`. It treats `age` and `ssh` as required, and asks for at least one of
`rsync`/`scp` and one of `docker`/`podman`:

```bash
nrg doctor
```

```
Energize Doctor

  ✓ Orchestration file found: Energize.rhai
  ✓ Energize.rhai compiles (2 function(s) defined)

  Tools:
  ✓ age found
  ✓ ssh found
  ✓ file transfer: rsync, scp found
  ✓ container runtime: docker found

  ✓ All checks passed!
```

`doctor` (like `nrg tasks`) only **compiles** the file — Rhai is dynamically typed, so this
catches syntax errors, not runtime or config errors. It does **not** run anything.

---

## Scaffold a file with `nrg init`

`nrg init` writes a starter `Energize.rhai` in the current directory. It refuses to
overwrite an existing one.

```bash
nrg init
```

The scaffold is a minimal, dependency-free script — no stdlib import, just two functions
over the SSH builtins:

```rhai
// Energize.rhai — Rhai orchestration module.
//
//   nrg run <fn> [args]   call a function defined here
//   nrg exec              run this file top-to-bottom
//   nrg exec --dry-run    show the plan without executing

let HOSTS = ["user@example.com"];

// `nrg run deploy`
fn deploy() {
    for host in HOSTS {
        let r = ssh_exec(host, "cd /var/www/app && git pull origin main");
        if !r.ok { throw "deploy failed on " + host + ": " + r.stderr; }
    }
    print("Deployed to all hosts.");
}

// `nrg run uptime`
fn uptime() {
    ssh_exec_all(HOSTS, "uptime");
}
```

List the entry points it defines (each `fn` is a `nrg run` target):

```bash
nrg tasks
```

```
Functions:
  deploy
  uptime
```

`nrg` discovers the file as `Energize.rhai` (or `energize.rhai`) in the current directory.
Pass an explicit path to `nrg exec <file>`, or `--file <path>` to
`nrg run` / `nrg tasks` / `nrg doctor`.

---

## `nrg exec` vs `nrg run`

Both commands build the same engine and run the **top level** of the file first (so
`import`s and top-level `let`/config statements execute, with their side effects). They
differ in what happens after that.

### `nrg exec [file]` — run a module top-to-bottom

`nrg exec` evaluates the whole file in order and stops. This is the right command when your
script **is** the deploy: top-level statements call into the stdlib and the deploy happens
as evaluation reaches them. The framework examples in `lib/examples/` are written this way.

```bash
nrg exec                 # runs ./Energize.rhai top-to-bottom
nrg exec deploy.rhai     # runs a specific file
```

### `nrg run <fn> [args...]` — call a function

`nrg run` runs the top level (imports, config), then **calls the named function**. Use it
when your file defines several entry points (like the `deploy` / `uptime` scaffold) and you
want to pick one.

```bash
nrg run deploy
nrg run uptime
```

Two things to know about arguments:

- **Every trailing CLI argument is passed as a Rhai _string_.** If your function needs a
  number, coerce it inside the function (e.g. `parse_int(n)`). There are no typed args.
- A function argument that itself starts with `-` must come after a `--` separator, so it
  isn't mistaken for a flag:

  ```bash
  nrg run scale web -- --replicas=3
  ```

`nrg run` refuses up front if no function by that name is defined — so it will never
accidentally run a top-level deploy while looking for a function that doesn't exist.

> **Gotcha — top level runs either way.** Because `nrg run` evaluates the top level first,
> any *side-effecting* statement you put at the top level (not inside a `fn`) runs even when
> you only meant to call one function. Keep top-level code to imports and configuration;
> put effects inside functions if you use `nrg run`.

---

## A first end-to-end example

Here is a small, self-contained `Energize.rhai` that shows both styles. It checks the fleet
with `ssh_exec_all`, then drives a real zero-downtime deploy through the stdlib.

This script **imports the stdlib**, so you must vendor `lib/` next to it (see
[Using the stdlib](#using-the-stdlib) below).

```rhai
// Energize.rhai

import "lib/runtime" as rt;
import "lib/deploy" as deploy;

// Pick the container runtime once. "auto" probes the local system
// (docker -> podman -> nerdctl; OrbStack is detected as a docker variant).
rt::set_runtime("auto");

let SERVICE   = "myapp";
let IMAGE     = "ghcr.io/myorg/myapp";
let VERSION   = env_or("DEPLOY_TAG", "latest");   // read $DEPLOY_TAG, default "latest"
let WEB_HOSTS = ["deploy@web1.example.com", "deploy@web2.example.com"];

// `nrg run status` — fan out a read-only command across the fleet in parallel.
fn status(hosts) {
    let results = ssh_exec_all(hosts, "uptime");
    for r in results {
        // ssh_exec_all never aborts on a single-host failure; each result carries its own .ok
        if r.ok {
            // trim() MUTATES in place and returns unit — don't use its return value.
            let line = r.stdout; line.trim();
            print(r.host + ": " + line);
        } else {
            print(r.host + ": UNREACHABLE (" + r.stderr + ")");
        }
    }
}

// `nrg run deploy` — zero-downtime rolling deploy via the stdlib.
fn deploy() {
    deploy::deploy(WEB_HOSTS, IMAGE + ":" + VERSION, SERVICE, #{
        container_port: 3000,
        envs: #{
            "RAILS_ENV":       "production",
            "SECRET_KEY_BASE": reveal(secret("SECRET_KEY_BASE")),
        },
        health_path: "/up",
    });
    print(SERVICE + " " + VERSION + " is live.");
}
```

Note a few things that the engine enforces — these are the most common beginner mistakes:

- **`import "lib/x" as x;` goes at the _top level_**, not inside a function. Paths resolve
  relative to the file's own directory.
- **Optional arguments are a config map `#{ … }`.** Rhai has no keyword args or default
  parameters; `deploy::deploy(...)` reads keys like `container_port`, `health_path`, `envs`
  out of the map.
- **A `Secret` can't be concatenated into a string.** `"key=" + secret("X")` throws. Put a
  `reveal(secret("X"))` into an env map (the revealed plaintext stays registered for
  redaction), or `sh_quote(secret("X"))` for a shell argument.
- **`trim()` mutates in place** and returns unit. `let s = r.stdout; s.trim(); s` gives the
  trimmed value; `let s = r.stdout.trim();` gives `()`.

Run it. Inspect, then deploy:

```bash
nrg tasks                          # status, deploy
DEPLOY_TAG=v1.0.0 nrg run status -- "deploy@web1.example.com"   # (1 string arg)
DEPLOY_TAG=v1.0.0 nrg run deploy --dry-run                      # preview, no changes
DEPLOY_TAG=v1.0.0 nrg run deploy                                # do it
```

(`status` here takes one argument; pass a single host string, or call it without the
`hosts` parameter from a wrapper. The point is to show that `nrg run` arguments arrive as
strings.)

### Using the stdlib

The stdlib is embedded in the `nrg` binary — `import "std/docker" as docker;` etc. works with
**zero setup**, no vendoring required, version-locked to the binary. Prefer this for a script you
write yourself.

The `lib/examples/*.rhai` files (below) predate this and still use the on-disk
`import "lib/…"` convention, so copying one needs the stdlib vendored as a **sibling**
directory (import paths resolve relative to the script's own directory):

```bash
cp lib/examples/rails.rhai ./Energize.rhai   # or write your own using import "std/…"
cp -r lib ./lib                              # vendor the stdlib next to it (or: nrg vendor)
```

`nrg vendor [--force]` does the same as `cp -r lib ./lib` — materializing the embedded stdlib
onto disk — and is only needed if you want to customize a module's behavior; edit the vendored
copy and switch that one import from `"std/X"` to `"lib/X"` (a real, on-disk file always takes
priority over the embedded copy).

The shipped modules are `runtime`, `docker`, `proxy`, `caddy`, `healthcheck`, `registry`,
`deploy`, and `recipe`. The headline entry point is `deploy::deploy(hosts, image, service, #{ … })` — a
fleet-atomic rolling update that builds, pushes, pulls, health-checks each new container,
switches kamal-proxy traffic, and unwinds the **whole fleet** if any host fails mid-roll.
See [stdlib.md](stdlib.md) and [deploy.md](deploy.md) for the details.

> kamal-proxy is the only proxy the stdlib drives. There is no nginx, Caddy, TLS, or
> provisioning module — Energize orchestrates an existing host, it does not provision one.

---

## Preview with `--dry-run`

Before any real run, plan it. `--dry-run` (available on both `nrg exec` and `nrg run`)
intercepts effects instead of performing them and prints the plan at the end.

```bash
nrg exec --dry-run
nrg run deploy --dry-run
```

```
PLAN (dry run — no changes made):
  ssh     deploy@web1            docker pull ghcr.io/myorg/myapp:v1.0.0
  ssh     deploy@web1            docker run -d --name myapp-web-v1.0.0-13000 ...
  ...
N action(s), M host(s). 0 executed.
```

What dry-run actually does, by effect type — worth knowing so the plan reads correctly:

- **Mutating builtins** (`ssh_exec`, `local_exec`, `write_remote`, container/proxy
  `sim_*` mutations, `state_set`/`state_del`) **record** a planned action and return a
  synthetic success (`ok`). They update an in-memory **simulation overlay**, so a stubbed
  `sim_docker_run` makes a later `sim_container_running` read return `true` — reads-after-writes
  stay consistent.
- **Reads** route through that overlay (seeded lazily from one real probe per container).
- **`http_get` / `http_post` short-circuit** to a healthy synthetic `200` — health checks
  pass in a plan.
- **`sleep` is skipped** (no real delay).
- A dry run takes **no state lock** and writes **no state**.

`ssh_probe` is read-only but still *runs* under `--dry-run` (it is not a mutation). Keep
that in mind if a probe touches something slow or sensitive.

> Dry-run is a **simulation, not a proof**. Behavior that depends on un-modeled remote state
> can still diverge from a real run. Use it to catch shape/ordering mistakes, not as a
> guarantee.

A live (non-dry) run does the opposite: it resolves the project root, takes an exclusive
advisory lock on `<root>/.energize/state.lock` for the duration, and persists state
atomically. Concurrent mutating runs serialize. See [safety.md](safety.md).

---

## Where to go next

- **[cli.md](cli.md)** — every command and flag (`exec`, `run`, `tasks`, `ssh`, `init`,
  `doctor`, `secrets`).
- **[builtins.md](builtins.md)** — the runtime builtins (`ssh_exec*`, `http_*`, `state_*`,
  `secret`/`reveal`/`sh_quote`, `transaction`/`on_rollback`, the `sim_*` family) with exact
  signatures.
- **[stdlib.md](stdlib.md)** — the `lib/` modules: runtime, docker, proxy, healthcheck,
  registry, deploy.
- **[deploy.md](deploy.md)** — how the fleet-atomic `deploy()` / `rollback()` /
  `accessory_run()` workflow behaves end-to-end.
- **[safety.md](safety.md)** — dry-run, state locking, atomic state writes, secret handling,
  and transactional rollback.
- **[authoring.md](authoring.md)** — writing valid Rhai for `nrg`: config maps, the `Secret`
  type, `state_get` returning `()` for absent keys, per-file imports, and other gotchas.
