# Energize (`nrg`)

A deployment toolkit written in Rust with a **Rhai** orchestration engine. You write
your deployment as a `.rhai` script; `nrg` evaluates it top-to-bottom, and the built-in
functions (`ssh_exec`, `http_get`, `state_set`, …) have real side effects as evaluation
reaches them. The shipped standard library turns that into a Kamal-style, **fleet-atomic,
zero-downtime** Docker deploy with automatic rollback.

There are two ways to run a script, over **one** engine:

- `nrg exec [file]` — evaluate a `.rhai` module top-to-bottom (defaults to `Energize.rhai`).
- `nrg run <fn> [args...]` — load the same file, then **call a function** defined in it.

## Quick Start

```bash
# Scaffold a starter Energize.rhai
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

## Documentation

This README is the overview. The full reference lives in [`docs/`](docs/):

| Guide | What it covers |
|---|---|
| [Getting Started](docs/getting-started.md) | Install, scaffold, your first deploy, `exec` vs `run`, `--dry-run` |
| [CLI Reference](docs/cli.md) | Every command and flag (`exec`/`run`/`tasks`/`init`/`doctor`/`ssh`/`secrets`) |
| [Builtins Reference](docs/builtins.md) | Every runtime builtin — signatures, return types, dry-run behavior |
| [Standard Library](docs/stdlib.md) | `runtime`/`docker`/`proxy`/`healthcheck`/`registry` module functions |
| [Fleet-Atomic Deploy](docs/deploy.md) | `deploy()` lifecycle, rollback, accessories, and the kamal-proxy choice |
| [Safety Features](docs/safety.md) | Dry-run, state locking, secrets, and transactions in depth |
| [Authoring Guide](docs/authoring.md) | Writing `Energize.rhai`: Rhai idioms and gotchas |
| [Architecture](docs/architecture.md) | Engine internals for contributors |
| [Framework Examples](docs/examples.md) | Rails / Django / Next.js / Phoenix / Laravel walkthroughs |

## Installation

Build from source (requires a recent stable Rust):

```bash
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

### Optional Dependencies

| Tool      | Required for                          | Install                                |
|-----------|---------------------------------------|----------------------------------------|
| `ssh`     | Remote execution                      | Part of OpenSSH (usually pre-installed) |
| `age`     | Secret encryption (`nrg secrets`)     | `brew install age` / `apt install age` |
| `rsync`   | File transfer (preferred)             | Usually pre-installed                  |
| `scp`     | File transfer (fallback)              | Part of OpenSSH                        |
| `docker`  | Container deployments                 | https://docs.docker.com/get-docker     |
| `podman`  | Container deployments (alternative)   | https://podman.io/getting-started      |
| OrbStack  | Container deployments (macOS)         | https://orbstack.dev                   |

`nrg doctor` checks for `age` and `ssh`, plus at least one of `rsync`/`scp` and one of
`docker`/`podman`.

---

## How it works

`nrg` builds a single Rhai engine, registers the runtime builtins (each capturing a shared
per-run context), and evaluates your file:

- `nrg exec Energize.rhai` runs the **module top level** — `import`s, top-level statements,
  and any side-effecting builtin calls run in order.
- `nrg run deploy arg1 arg2` first evaluates the top level (so `import`s and config run),
  then **calls** the script function `deploy`. Each trailing CLI argument is passed as a
  **Rhai string** — the function decides how to coerce it. `nrg run` refuses up front if no
  such function is defined, so it never accidentally runs a top-level deploy while looking
  for a missing function.

`nrg tasks` and `nrg doctor` only *compile* the file (Rhai is dynamically typed, so this
catches syntax errors, not runtime/config errors) — they don't run it.

The orchestration file is discovered as `Energize.rhai` (or `energize.rhai`) in the current
directory; pass an explicit path to `nrg exec <file>` or `--file <path>` to `nrg run`/`nrg
tasks`/`nrg doctor`.

---

## Runtime builtins

These functions are registered into every `.rhai` file `nrg` runs — at the top level **and**
inside imported module functions.

### Execution

| Function | Signature | Notes |
|---|---|---|
| `ssh_exec` | `(host, cmd) -> ExecResult` | Run a command on a host via SSH. **Mutating.** |
| `ssh_probe` | `(host, cmd) -> ExecResult` | Read-only remote command (still runs under `--dry-run`). |
| `local_exec` | `(cmd) -> ExecResult` | Run a command locally via `sh -c`. **Mutating.** |
| `ssh_exec_all` | `(hosts, cmd) -> [ExecResult]` | Fan out across hosts **in parallel**. Never aborts on a single-host failure (each result carries its own `.ok`). Non-string host elements are rejected loudly. |
| `ssh_exec_stdin` | `(host, cmd, stdin) -> ExecResult` | Like `ssh_exec`, but delivers `stdin` **off-argv** (e.g. a password to `--password-stdin`). The payload is never traced or placed on the command line. |
| `local_exec_stdin` | `(cmd, stdin) -> ExecResult` | Local mirror of `ssh_exec_stdin`. |
| `write_remote` | `(host, content, remote_path) -> ExecResult` | Write `content` to a `0600` remote file via stdin (`umask 077; cat > …`). Content never touches argv. |

**`ExecResult`** has read-only getters: `stdout`, `stderr`, `exit_code`, `host`, and `ok`
(`ok == (exit_code == 0)`). Getters are read-only — you can't assign `r.ok = …`.

### HTTP

| Function | Signature | Notes |
|---|---|---|
| `http_get` | `(url) -> HttpResponse` | HTTP GET, 30s timeout. Under `--dry-run`, short-circuits to a synthetic `200`. |
| `http_post` | `(url, body) -> HttpResponse` | HTTP POST with `Content-Type: application/json`. Synthetic `200` under `--dry-run`. |

**`HttpResponse`** has read-only getters: `status`, `body`, and `ok` (`ok` is true for any
`2xx`).

### State

State persists in `<project-root>/.energize/state.json`.

| Function | Signature | Notes |
|---|---|---|
| `state_get` | `(key) -> String \| ()` | Returns `()` (unit) when the key is **absent**. |
| `has_state` | `(key) -> bool` | Presence check — use this in `if`, not `state_get`. |
| `state_set` | `(key, value)` | Persists atomically (records-only under `--dry-run`). |
| `state_del` | `(key)` | Delete a key; persists atomically. |
| `state_all` | `() -> Map` | Read the whole map. |

### Secrets

| Function | Signature | Notes |
|---|---|---|
| `secret` | `(name) -> Secret` | Resolve a secret (see [Secrets](#secrets)). Throws if missing or shorter than 6 chars. |
| `reveal` | `(Secret) -> String` | Explicitly un-wrap a `Secret` to plaintext (still registered for redaction). |
| `sh_quote` | `(String \| Secret) -> String` | POSIX single-quote a value safely (handles `'`, newlines, `$`, backticks). The only safe way to put a secret on a shell argument. |

A `Secret` cannot be concatenated into a string — `"x" + secret("Y")` **throws**. Use
`sh_quote(secret("Y"))` for a shell argument, or `reveal(secret("Y"))` for explicit
plaintext. Its `to_string()` is `***`.

### Transactions

| Function | Signature | Notes |
|---|---|---|
| `transaction` | `(\|\| { … })` | Run a body; if it `throw`s, unwind the compensations registered during it (LIFO, best-effort), then re-raise. |
| `on_rollback` | `(\|\| { … })` | Register a compensation (an inverse effect) on the active transaction's stack. |

### Container / port / health simulation builtins

The stdlib routes every container existence/state/health **read** and container/proxy
**mutation** through these typed `sim_*` builtins, so `--dry-run` stays self-consistent
(a stubbed `sim_docker_run` makes `sim_container_running`/`sim_container_healthy` true).
In **live** mode they run the real command / probe.

| Function | Signature |
|---|---|
| `sim_container_running` | `(host, name) -> bool` |
| `sim_container_healthy` | `(host, name) -> bool` |
| `sim_image_id` | `(host, tag) -> String` |
| `sim_pick_port` | `(host, base) -> int` |
| `sim_wait_port` | `(host, port) -> bool` |
| `sim_docker_run` | `(host, tag, name, cmd) -> ExecResult` |
| `sim_docker_stop` | `(host, name, cmd) -> ExecResult` |
| `sim_docker_rename` | `(host, old, new, cmd) -> ExecResult` |
| `sim_docker_remove` | `(host, name, cmd) -> ExecResult` |
| `sim_proxy_switch` | `(host, service, target, cmd) -> ExecResult` |

### Utilities

| Function | Signature | Notes |
|---|---|---|
| `sleep` | `(seconds)` | Blocking delay (integer seconds). No-op under `--dry-run`. |
| `nrg_env` | `(name) -> String` | Read an env var; **throws** if unset. |
| `env_or` | `(name, default) -> String` | Read an env var, with a fallback default. |
| `join` | `(array, sep) -> String` | Join array elements with a separator (Rhai core has no `join`). |
| `is_dry_run` | `() -> bool` | True during a `--dry-run` run (used for cosmetic branches, e.g. printing `<auto>` ports). |

`print(...)` and `debug(...)` go to **stderr**, with registered secrets redacted.

---

## Safety features

`nrg` adds four production-safety guarantees on top of the side-effecting evaluation model.

### `--dry-run` — plan without performing

`nrg exec --dry-run` and `nrg run <fn> --dry-run` intercept effects instead of executing
them: mutating builtins **record** a planned action and update an in-memory **simulation
overlay** (seeded lazily from one real probe per container, then updated by stubbed writes,
so reads-after-writes stay consistent), and return a synthetic `ok`. Reads route through the
overlay; `http_get`/`http_post` short-circuit to healthy `200`. A dry run takes **no lock**,
writes **no state**, and ends by printing the plan:

```
PLAN (dry run — no changes made):
  ssh     deploy@web1            docker pull ghcr.io/org/app:v42
  ssh     deploy@web1            docker run -d --name app-web-v42-13000 ...
  ...
N action(s), M host(s). 0 executed.
```

Dry-run is a *simulation*, not a proof: behavior that depends on un-modeled remote state can
still diverge from a real run.

### State locking

Before a live run, `nrg` resolves the **project root** (the nearest ancestor directory
containing a marker — `.energize/`, `energize.toml`, or `.nrg-key` — never `.git`, and never
above `$HOME`), then takes an **exclusive advisory `flock`** on `<root>/.energize/state.lock`
for the duration of the run. Concurrent mutating runs serialize (the second waits, with a
message). Nested `nrg` invocations under the same root **re-enter** the existing lock instead
of self-deadlocking. Dry runs take no lock.

State writes are **atomic**: write `state.json.tmp` → `fsync` → `rename`, keeping a
`state.json.bak`. A **missing** state file is an empty store (legitimate first run); a
**present-but-corrupt** file is **fatal** — `nrg` refuses to run rather than silently zeroing
your deploy history.

### Secrets

`secret("NAME")` returns a provenance-tagged `Secret`, looked up from `$NRG_SECRET_<NAME>`,
then `.energize/secrets`, then `.env`. Secrets shorter than 6 chars are rejected.

- A `Secret` can't be stringified into a command (concatenation throws); its `Debug`/`to_string`
  render as `***`.
- Passwords are delivered **off-argv** via `--password-stdin` (`ssh_exec_stdin` /
  `local_exec_stdin`), so the plaintext never appears in argv, in the command string, or in
  the dry-run plan.
- Registered secret values are **redacted** from `print`/`debug` output, the trace, thrown
  errors, and the plan log.

Encrypted-secret management uses [age](https://github.com/FiloSottile/age):

```bash
nrg secrets init               # generate .nrg-key / .nrg-key.pub
nrg secrets encrypt "value"    # -> an ENC[...] token
nrg secrets decrypt 'ENC[...]' # -> plaintext
nrg secrets seal .env          # encrypt a .env file -> .env.enc
nrg secrets unseal .env.enc    # decrypt for editing -> .env
```

### Transactions / rollback

`transaction(|| { … })` runs its body; if it `throw`s, every `on_rollback(|| { … })`
compensation registered so far is unwound **LIFO**, **best-effort** and **error-isolated**
(a compensation that itself throws is logged and the unwind continues), then the original
error re-raises. Register the inverse *before* the effect, and make it idempotent (e.g.
`rm -f`, `… || true`), so it tolerates "the effect never happened". This is what makes the
stdlib's `deploy()` fleet-atomic.

---

## Standard library (`lib/`)

The shipped stdlib is a set of `.rhai` modules:

| Module | Purpose |
|---|---|
| `lib/runtime.rhai` | Container runtime abstraction (docker / podman / orbstack / nerdctl). |
| `lib/docker.rhai` | Image build/push/pull, container run/stop/remove/rename, inspect, logs, cleanup. |
| `lib/proxy.rhai` | kamal-proxy boot + zero-downtime traffic switch (the default). |
| `lib/caddy.rhai` | Caddy boot + zero-downtime traffic switch via the admin API (same surface as `proxy.rhai`). |
| `lib/healthcheck.rhai` | HTTP / TCP-port / container-health retry loops. |
| `lib/registry.rhai` | Container registry login (off-argv password), AWS ECR. |
| `lib/deploy.rhai` | Fleet-atomic, zero-downtime `deploy()` / `rollback()` / `accessory_run()`. |

### Conventions

**Import with the `import "lib/x" as x;` form.** Paths resolve relative to the directory of
the file being executed, so `import "lib/docker" as docker;` loads `<file-dir>/lib/docker.rhai`.
The runtime builtins are in scope inside imported module functions too. Imports are
**per-file** in Rhai (a module is *not* inherited by the caller's imports), so a module
imports every other module it touches directly.

**Optional arguments are a config map `#{ … }`** — Rhai has no keyword args or default
parameters. Functions read `cfg.contains("key")` with a fallback:

```rhai
import "lib/docker" as docker;
docker::docker_run(host, "ghcr.io/org/app:v42", "app", #{
    ports:   #{ "13000": "3000" },
    envs:    #{ "RAILS_ENV": "production" },
    volumes: #{},
});
```

**Examples import `lib/…`, which needs vendoring as a sibling.** The stdlib itself is embedded
in the `nrg` binary (`import "std/…"` works with zero setup — see below), but the example files
under `lib/examples/` were written against the on-disk `import "lib/…"` convention, so copying
one still needs `lib/` vendored next to it (the example's imports resolve relative to the
example file's own directory):

```bash
cp lib/examples/rails.rhai ./Energize.rhai
cp -r lib ./lib            # vendor the stdlib as a sibling of Energize.rhai
# or: nrg vendor           # does the same, from inside your project directory
```

For a project you're writing from scratch, prefer `import "std/docker" as docker;` over
`import "lib/docker" as docker;` — it resolves from the embedded, version-locked stdlib with no
vendoring at all. `nrg vendor` only matters if you want to customize a module's behavior (see
[`docs/cli.md`](docs/cli.md#nrg-vendor)).

### Container runtimes

`lib/runtime.rhai` lets you pick the container CLI once; every other module reads it (the
choice is shared across the per-file module instances via the state store):

```rhai
import "lib/runtime" as rt;
rt::set_runtime("auto");     // docker -> podman -> nerdctl; detects OrbStack as a docker variant
// rt::set_runtime("podman"); // or force one explicitly
```

`set_runtime("auto")` probes the local system with `local_exec`. Note: under `--dry-run`
`local_exec` is stubbed, so auto-detect resolves to `"docker"` in a plan — call
`set_runtime("docker")` explicitly if you need a deterministic plan for another runtime.

---

## Fleet-atomic deploy

`lib/deploy.rhai`'s `deploy()` is the headline workflow — a single-transaction rolling model:

```rhai
import "lib/deploy" as deploy;
deploy::deploy(WEB_HOSTS, "ghcr.io/org/app:v42", "app", #{
    container_port: 3000,
    envs:           #{ /* ... */ },
    health_path:    "/up",
    pre_deploy_cmd: "...",   // e.g. run migrations before switching traffic
});
```

What it does:

1. **Outside** the transaction: build → push → pull on all hosts → ensure proxy is up →
   snapshot the previous image to `<service>.prev` (so `rollback()` has a target).
2. **Inside one transaction wrapping the whole fleet**, per host *sequentially*: start the
   **new** container under a unique name on a fresh port, wait for HTTP health, register the
   rollback compensations (restore proxy → old target; `rm -f` new container), then switch
   proxy traffic to the new container. The **old container is kept running** under its
   canonical name throughout, so a rollback can flip the proxy straight back to it.
3. If any host fails mid-roll, the transaction unwinds the **whole fleet** best-effort —
   every already-switched host's proxy is restored to its snapshotted old target and the new
   containers are removed — then re-raises. The fleet is never left half-deployed.
4. **Post-commit** (only after the full fleet is up): one cleanup pass — promote new →
   canonical, retire the old container, prune — and persist `<service>.version` / `.image` /
   per-host proxy target.

Rolling per-host SSH is **sequential** (not `ssh_exec_all`), because fan-out swallows per-host
failures, which would defeat the atomic unwind. The rolling deploy is *flattened-atomic*, not
distributed-atomic: a mid-fleet failure unwinds touched hosts best-effort.

### Choosing the proxy

`deploy()` is proxy-agnostic. `cfg.proxy` selects the backend:

```rhai
deploy::deploy(WEB_HOSTS, "ghcr.io/org/app:v42", "app", #{
    proxy:  "caddy",                 // "kamal" (default) | "caddy"
    domain: "app.example.com",       // Caddy: adds a host match -> automatic HTTPS
});
```

- **`"kamal"`** (default, `lib/proxy.rhai`) — kamal-proxy does the health-gated atomic cutover
  in one command, which matches the rolling model exactly.
- **`"caddy"`** (`lib/caddy.rhai`) — runs Caddy with its admin API; the traffic switch is an
  atomic admin-API call that replaces the service route's upstream. `cfg.domain` enables
  automatic Let's Encrypt TLS.

Both modules expose the same `proxy_boot` / `proxy_deploy` / `proxy_remove` surface, so you can
drop in your own (nginx, Traefik, …) the same way — write a `lib/<name>.rhai` with that surface.

Companion functions:

- `deploy::rollback(hosts, service, #{ image: "" })` — redeploy a previous image (empty
  `image` uses the snapshotted `<service>.prev`), skipping build & push.
- `deploy::accessory_run(host, name, image, #{ ports, envs, volumes, network, cmd })` — start
  a long-lived accessory (Postgres, Redis, …) if it isn't already running.

---

## Authoring notes / gotchas

Things users hit when writing `.rhai` for `nrg`:

- **`trim()` mutates in place.** `let s = r.stdout; s.trim(); s` — `trim()` returns unit and
  edits `s`; it does not return the trimmed string.
- **No keyword args or default parameters.** Use a `#{}` config map for options; define
  multiple `fn` arities for "defaults" (e.g. `fn deploy(h,i,s)` calling `fn deploy(h,i,s,#{})`).
- **`state_get` of an absent key is `()` (unit), not `""`.** Test presence with
  `has_state(k)` or `state_get(k) != ()` — `if state_get(k) { … }` raises a type error
  (Rhai conditions must be `bool`).
- **A `Secret` can't be concatenated.** `"url=" + secret("X")` throws. Use
  `sh_quote(secret("X"))` for a shell argument, or `reveal(secret("X"))` for explicit
  plaintext (e.g. into an env map). Pass the raw `Secret` to `registry_login` so it streams to
  `--password-stdin` off-argv.
- **`nrg run` arguments are strings.** Every CLI arg becomes a Rhai string; coerce inside the
  function (`parse_int(arg)`, etc.) if you need a number.
- **`import` is per-file.** A module must `import` everything it uses; it doesn't inherit the
  caller's imports.
- **Rhai core has no `Array::join`** — use the host `join(arr, sep)` builtin.

---

## Framework examples

`lib/examples/` contains complete, production-shaped deployment scripts. Each is a full
`Energize.rhai` you copy and customize (remember to vendor `lib/` alongside it).

| Framework | File | App port | Health path |
|---|---|---|---|
| Ruby on Rails | `lib/examples/rails.rhai` | 3000 | `/up` |
| Django | `lib/examples/django.rhai` | 8000 | `/health/` |
| Next.js | `lib/examples/nextjs.rhai` | 3000 | `/api/health` |
| Phoenix | `lib/examples/phoenix.rhai` | 4000 | `/health` |
| Laravel | `lib/examples/laravel.rhai` | 8000 | `/up` |
| Generic | `lib/examples/Energize.rhai` | 3000 | `/up` |

```bash
# Copy an example + vendor the stdlib
cp lib/examples/rails.rhai ./Energize.rhai
cp -r lib ./lib

# Edit hosts, image, and service in Energize.rhai
$EDITOR Energize.rhai

# Provide secrets (env / .env / .energize/secrets)
export NRG_SECRET_REGISTRY_PASSWORD="ghp_xxxx"
export NRG_SECRET_DATABASE_URL="postgres://..."
export NRG_SECRET_SECRET_KEY_BASE="abc123..."

# Preview, then deploy
DEPLOY_TAG=v1.0.0 nrg exec --dry-run
DEPLOY_TAG=v1.0.0 nrg exec
```

---

## Commands

| Command | Description |
|---|---|
| `nrg exec [file]` | Evaluate a `.rhai` module top-to-bottom (defaults to `Energize.rhai`). `--dry-run` to plan. |
| `nrg run <fn> [args...]` | Call a function defined in the orchestration file. `--file` / `--dry-run`. |
| `nrg tasks` | List the functions defined in the orchestration file. `--file`. |
| `nrg ssh <host>` | Open an interactive SSH session (resolving `~/.ssh/config` aliases). |
| `nrg init` | Scaffold a starter `Energize.rhai`. |
| `nrg doctor` | Check the file compiles and required tools are on `PATH`. `--file`. |
| `nrg secrets <cmd>` | Manage encrypted secrets (`init` / `encrypt` / `decrypt` / `seal` / `unseal`). |

Set `NRG_TRACE=1` to trace each builtin invocation to stderr (with secrets redacted).

## SSH config integration

`nrg` reads `~/.ssh/config` and resolves host aliases automatically — the same names your
orchestration scripts use work with `nrg ssh`.

## License

MIT
