---
title: Standard Library
nav_order: 5
---

# Standard Library Reference

Energize ships a small Rhai standard library under `lib/`. These modules are
thin, readable wrappers over the runtime's built-in primitives (`ssh_exec`,
`local_exec`, the `sim_*` family, `http_get`, `state_*`, `secret`/`reveal`,
etc.). You import them at the top level of your `Energize.rhai` and call their
functions with `module::function(...)`.

This page documents these modules:

- [`lib/runtime`](#libruntime--container-runtime-selection) — pick the container CLI (docker/podman/nerdctl)
- [`lib/docker`](#libdocker--container-lifecycle) — build / push / pull / run / stop / inspect
- [`lib/proxy`](#libproxy--kamal-proxy) — kamal-proxy zero-downtime traffic switching
- [`lib/healthcheck`](#libhealthcheck--readiness-polling) — HTTP / TCP / container-health polling
- [`lib/registry`](#libregistry--registry-authentication) — registry login (incl. AWS ECR)

> `lib/deploy` (the high-level orchestrator that ties these together) has its
> own page.

All the snippets below are valid Rhai. A few conventions to keep in mind:

- `import "lib/x" as x;` must appear at the **top level** of the script, never
  inside a function.
- Config is passed as a Rhai object map literal: `#{ key: value }`. There are
  **no keyword arguments** — every option lives in the map.
- Failures `throw` (Rhai has no `fail`); wrap calls in `try { ... } catch (e) { ... }`
  if you want to recover.
- A `Secret` (from `secret("ENV_VAR")`) **cannot be concatenated** with a
  string — `"x" + my_secret` throws. Use `reveal(secret)` or `sh_quote(secret)`
  to get a `String` at the last possible moment.
- `state_get(key)` returns the unit value `()` when the key is absent; guard
  with `has_state(key)` first.

---

## Dry-run model (read this first)

Every mutating module function ultimately calls a built-in. Those built-ins
fall into classes that behave differently under `nrg --dry-run`:

| Built-in class | Examples | Dry-run behavior |
| --- | --- | --- |
| **Mutating exec** | `ssh_exec`, `local_exec`, `ssh_exec_stdin`, `local_exec_stdin` | Command is **recorded** into the plan; returns a synthetic `ok=true`, empty stdout. The command does **not** actually run. |
| **Container sim** | `sim_docker_run`, `sim_docker_stop`, `sim_docker_remove`, `sim_docker_rename`, `sim_proxy_switch` | Recorded **and** applied to an in-memory container overlay, so later reads stay consistent. |
| **Container reads** | `sim_container_running`, `sim_image_id`, `sim_wait_port`, `sim_container_healthy` | Read from the overlay (reflecting earlier sim mutations), not the live host. |
| **HTTP** | `http_get`, `http_post` | Short-circuits to a synthetic `200` response. |
| **Timing** | `sleep` | Skipped entirely (no wall-clock wait). |

The key invariant the library upholds: **every container read or mutation that
feeds a later decision goes through a `sim_*` built-in**, never a raw
`docker inspect` / `nc -z` over `ssh_exec`. That is what makes a dry run a
faithful preview — the simulated container world is internally consistent. Raw
`ssh_exec` is used only for effects whose result is not branched on later
(pull, prune, exec-into, logs).

---

## `lib/runtime` — Container runtime selection

```rhai
import "lib/runtime" as rt;
```

Picks which container CLI every other module uses, so you configure it in one
place. Supported runtimes:

| Value | Meaning |
| --- | --- |
| `"docker"` | Docker CE/EE — also OrbStack, Rancher Desktop, colima, any Docker-compatible CLI. **Default.** |
| `"podman"` | Podman (rootful or rootless). |
| `"nerdctl"` | nerdctl (containerd). Experimental. |
| `"auto"` | Probe the local system: try docker, then podman, then nerdctl. |

### How the selection is stored (session-backed, mirrored to state)

In Rhai, **every `import` yields a fresh module instance**. A module-global
variable in `runtime.rhai` would therefore not be visible to `docker.rhai`,
`proxy.rhai`, etc. So the runtime choice this run resolves to is stored in the
**ephemeral, in-memory-only `session` store** (see
[`session_set`/`session_get`/`has_session`](authoring.md#ephemeral-per-run-session-store-session_setsession_gethas_session)
in the Authoring Guide) under two keys, which all imports within this run share:

- `nrg.runtime.cmd` — the CLI command string (e.g. `"docker"`, `"podman"`).
- `nrg.runtime.name` — the human-readable name (may be `"orbstack"`).

`container_cmd()` and `runtime_name()` read these keys from `session`
(defaulting to `"docker"` when unset), so any module that calls
`rt::container_cmd()` sees your choice — as long as you call `set_runtime(...)`
**before** invoking other library functions, and only for **this** run.

`set_runtime()`/`auto_detect()` additionally mirror the same two keys into the
durable `state_set` store, purely so `nrg status`/`nrg logs`/`nrg app exec` —
separate CLI invocations that never re-run your `Energize.rhai` — can later
recover which runtime a past deploy used. `container_cmd()`/`runtime_name()`
never read that durable copy: robustness review R27 found that doing so made a
runtime choice from a **previous** run silently override this run's true
default the moment it stopped calling `set_runtime()`.

### Functions

#### `set_runtime(runtime)` / `set_runtime()`

Sets the runtime for all subsequent operations. `runtime` is one of `"docker"`,
`"podman"`, `"nerdctl"`, `"auto"`. The 0-arg overload defaults to `"auto"`
(Rhai has no default params). Throws `"Unknown container runtime: ..."` on an
unrecognized value. Prints the resolved runtime, e.g.
`[nrg] container runtime: docker (docker)`.

```rhai
import "lib/runtime" as rt;

rt::set_runtime("podman");   // explicit
// or
rt::set_runtime("auto");     // probe
// or
rt::set_runtime();           // same as "auto"
```

#### `container_cmd() -> String`

The container CLI command (e.g. `"docker"`). Reads `nrg.runtime.cmd` from the
ephemeral per-run `session` store, defaulting to `"docker"` if `set_runtime()`
was never called THIS run. This is what library modules concatenate when
building commands.

#### `runtime_name() -> String`

The human-readable runtime name. Same as `container_cmd()` except it can return
`"orbstack"` when auto-detect identifies OrbStack.

#### `is_docker() -> bool` / `is_podman() -> bool`

`is_docker()` is true when the name is `"docker"` (note: it returns **false**
for `"orbstack"`, since that comparison is exact). `is_podman()` is true when
the name is `"podman"`.

#### `runtime_run_flags() -> String`

Extra flags appended to `run` commands. Currently always
`"--restart unless-stopped"`.

#### `runtime_exec_cmd(container_name, command) -> String`

Builds a `<cmd> exec <container_name> <command>` string. (Helper; not used by
the other modules, which build their own exec strings.)

### Auto-detect gotcha (dry-run)

`auto_detect()` (invoked by `set_runtime("auto")`) probes with `local_exec`,
which is a **mutating-class** built-in. Under `--dry-run`, `local_exec` records
and returns a synthetic `ok=true` instead of really running. That makes the
**first** probe branch (`docker info ...`) match unconditionally, so a dry-run
auto-detect always resolves to `"docker"` (the safe default), never actually
probing for podman/nerdctl.

If you need a real probe, call `set_runtime("docker")` (or `"podman"`, etc.)
explicitly, or run auto-detect in a live (non-dry-run) invocation.

---

## `lib/docker` — Container lifecycle

```rhai
import "lib/docker" as docker;
```

Build, push, pull, run, stop, remove, rename, inspect, clean up, exec, and tail
logs. Builds run locally (`local_exec`); everything else runs on a remote `host`
via SSH. The runtime command comes from `lib/runtime`, so these work with
docker / podman / orbstack / nerdctl.

> All container **reads and state-changing mutations route through `sim_*`**
> built-ins (see the dry-run table above). Raw `ssh_exec` is used only for
> pull, prune, exec, and logs.

### Build & push (local machine)

#### `docker_build(tag, cfg)` / `docker_build(tag)`

Builds an image locally. Returns the build `ExecResult`; **throws** on failure
(`"<cmd> build failed:\n<stderr>"`).

`cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `context` | `"."` | Build context path. |
| `dockerfile` | `"Dockerfile"` | Path to the Dockerfile (`-f`). |
| `build_args` | `#{}` | Map of `--build-arg KEY=VALUE` pairs. |
| `platform` | `""` | A single target platform (e.g. `"linux/amd64"`) other than the build machine's own, or a comma-separated list (e.g. `"linux/amd64,linux/arm64"`) for a genuine multi-platform manifest list. A single platform uses `buildx build --platform <value> --load` instead of a plain `build` — needed when building on, say, an Apple Silicon laptop for an x86 VPS; `--load` keeps the result a normal local image, so the separate `docker_push` step still works. A comma-separated list uses `--push` instead, publishing the manifest list straight to the registry during the build. Docker/Podman only — nerdctl has no `buildx` subcommand. |

```rhai
docker::docker_build("ghcr.io/me/app:v1", #{
    context: ".",
    dockerfile: "Dockerfile",
    build_args: #{ MIX_ENV: "prod" },
    platform: "linux/amd64",   // building on an ARM laptop, deploying to an x86 host
});
```

#### `docker_push(tag)`

Pushes `tag` to its registry from the local machine. Returns the push
`ExecResult`; throws on failure.

### Pull (remote hosts)

#### `docker_pull(host, tag)`

Pulls `tag` on a single remote `host`. Throws on failure.

#### `docker_pull_all(hosts, tag)`

Pulls `tag` on all `hosts` in parallel (via `ssh_exec_all`). Returns the list of
`ExecResult`; throws listing the failed hosts.

```rhai
docker::docker_pull_all(["10.0.0.1", "10.0.0.2"], "ghcr.io/me/app:v1");
```

### Run

#### `docker_run(host, tag, name, cfg)` / `docker_run(host, tag, name)`

Builds a `<cmd> run -d ...` command and routes it through `sim_docker_run`, so a
dry run records the new container as running + healthy in the overlay. Returns
the `ExecResult` (the caller checks `.ok` — it does **not** throw here).

`cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `ports` | `#{}` | Map of `host_port: container_port` → `-p host:container` (sh-quoted). |
| `envs` | `#{}` | Map of env vars → a **0600 remote env-file** delivered off-argv + `--env-file` (never `-e KEY=VALUE`). |
| `volumes` | `#{}` | Map of `host_path: container_path` → `-v host:container` (sh-quoted). |
| `network` | `""` | Adds `--network <value>` (sh-quoted) when non-empty. |
| `extra` | `""` | Raw extra args appended verbatim (the one un-quoted escape hatch — keep secrets out). |

`--restart unless-stopped` (from `runtime_run_flags()`) and `--name <name>` are
always included.

```rhai
let r = docker::docker_run(host, "ghcr.io/me/app:v1", "app-green", #{
    ports:   #{ "3000": "3000" },
    envs:    #{ PHX_SERVER: "true" },
    volumes: #{ "/srv/app/uploads": "/app/uploads" },
    network: "host",
});
if !r.ok { throw "run failed: " + r.stderr; }
```

> **Note on env values & secrets:** `envs` are written to a **0600 remote env-file**
> via the off-argv stdin channel and passed with `--env-file`, so they never appear
> on the remote argv (`ps -ef`, `docker inspect`). Put a `reveal(secret("X"))` into
> `envs` for a secret value (the revealed plaintext stays registered for redaction);
> the raw `Secret` itself can't be string-concatenated. Every other interpolated
> value (name, tag, ports, volumes, network) is `sh_quote`'d. The `extra` field is
> the only verbatim passthrough — keep secrets out of it. Each key/value is
> validated before the env-file is built (robustness review R19): a key or value
> containing a newline throws (it would otherwise inject an extra `KEY=VALUE`
> line into the file — base64-encode a multi-line value like a PEM key instead),
> and a key containing `=` throws too.

### Stop / remove / rename (sim-routed mutations)

#### `docker_stop(host, name, cfg)` / `docker_stop(host, name)`

Stops a running container. Routed through `sim_docker_stop`. `cfg` key
`timeout` (default `30`) → `stop -t <timeout>`. The command is suffixed with
`2>/dev/null || true`, so it never fails the deploy if the container is absent.

#### `docker_remove(host, name)`

Removes a container (`rm -f`), routed through `sim_docker_remove`. Also
`|| true`.

#### `docker_rename(host, old_name, new_name)`

Renames a container, routed through `sim_docker_rename`. Unlike
`docker_stop`/`docker_remove`, this does **not** blanket-swallow docker-level
failure with `|| true` (Fable review, full-project pass): only "no such
container" (matched case-insensitively across stdout+stderr, so it catches
both Docker's and Podman's differently-capitalized/-phrased wording) — the
idempotent case where a previous partial run already completed this exact
rename — is downgraded to `ok: true`; any other failure (a name conflict with
an existing container, permission denied, daemon unreachable, ...) is
returned as `ok: false` with the real `stderr`, since those mean the swap did
NOT happen and must not be treated as success.

#### `docker_network_create(host, name)` / `docker_network_create_all(hosts, name)`

Create a user-defined network on a host, idempotently (roadmap 1.5, `nrg
setup`) — a plain `ssh_exec`, not `sim_*`-routed, since nothing in this
codebase reads back "does this network exist" within a single dry run.
`"already exists"` (matched case-insensitively across stdout+stderr, same
convention as `docker_rename`'s "no such container" classification) is
treated as success; any other failure is a real `ok: false`.
`docker_network_create_all` loops over `hosts`, throwing (naming the host) on
the first real failure. Nothing else in this codebase creates or joins a
network automatically — pass the same name as `cfg.network` to `deploy()`/
`accessory_run` calls that should use it.

### Inspection (sim-routed reads)

#### `docker_container_running(host, name) -> bool`

True if the container exists and is running. Reads `sim_container_running` so
dry-run stays consistent with simulated mutations.

#### `docker_image_id(host, tag) -> String`

The image id for a tag, or `""` if not found. Reads `sim_image_id`.

### Cleanup, exec, logs

#### `docker_cleanup(host, cfg)` / `docker_cleanup(host)`

Prunes exited containers and dangling images via two `ssh_exec` calls (both
`|| true`). Returns the image-prune `ExecResult`. `cfg` is reserved and
currently unused by this function.

> Note: pruning a repo's own old TAGGED images (as opposed to dangling ones) is
> a separate, opt-in concern — see `docker_prune_old_images` below, and
> `deploy()`'s own `cfg.keep_images` key in `docs/deploy.md` (robustness review
> R22).

#### `docker_prune_old_images(host, repo, keep_n, protect_tags)`

Lists `repo`'s tags on `host` (`docker images <repo> --format
'{{.Tag}}|{{.CreatedAt}}'`, a raw `ssh_exec` — a prune has no later read to
diverge from, so it's exempt from the `sim_*` CONTAINER-OVERLAY CONTRACT above)
and removes every tag beyond the `keep_n` most recently created, except any tag
in `protect_tags` (kept regardless of age). Returns `#{ok, removed, stderr}`;
`ok: false` (with `removed: []`) only when the listing itself fails — never
guessed at by acting on incomplete data. Never throws: an image still
referenced by a container fails to remove via Docker/Podman's own safety net,
masked by `|| true` same as `docker_cleanup`. Called from `deploy()`'s
post-commit loop when `cfg.keep_images` is set (see `docs/deploy.md`), not
meant to be called directly in normal use.

#### `docker_exec(host, name, command)`

Runs `command` inside a running container via plain `ssh_exec` (the result is
not branched on, so it doesn't need the sim).

#### `docker_logs(host, name, cfg)` / `docker_logs(host, name)`

Tails recent logs. `cfg` key `tail` (default `100`) → `logs --tail <n>`. Output
is merged (`2>&1`).

```rhai
let logs = docker::docker_logs(host, "app", #{ tail: 200 });
print(logs.stdout);
```

---

## `lib/proxy` — kamal-proxy

```rhai
import "lib/proxy" as proxy;
```

Wraps [kamal-proxy](https://github.com/basecamp/kamal-proxy), a lightweight
reverse proxy that drains connections from the old container before switching
traffic to the new one — the mechanism behind zero-downtime deploys.

> **The proxy backend is pluggable.** kamal-proxy (`lib/proxy`) is the default,
> and a **Caddy** backend (`lib/caddy`) ships alongside it — select it with
> `deploy(..., #{ proxy: "caddy" })`. Both expose the same surface
> (`proxy_boot` / `proxy_deploy` / `proxy_remove` / `proxy_set_tls` / `proxy_list`
> / `proxy_stop` / `proxy_boot_all`), so you can drop in your own (nginx, traefik,
> …) by writing a module with the same functions. See `lib/caddy` below and the
> Caddy section in `docs/deploy.md`.
>
> **Proxy seam contract:** `deploy()` builds one `proxy_cfg` map
> (`#{ health_path, domain }`) and passes it IDENTICALLY to the forward traffic
> switch and the rollback restore. A backend should honor `health_path`
> (health-gate / actively health-check the cutover) and `domain` (TLS host match).
> kamal-proxy uses `health_path`; Caddy uses both (host match + active health check).

Constants used internally:

- Image: `basecamp/kamal-proxy:latest`
- Container name: `kamal-proxy`

The proxy runs with `--network host` (so it can bind 80/443) and persists its
config to the named volume `kamal-proxy-config`.

> The running-check reads through `sim_container_running` and the traffic switch
> goes through `sim_proxy_switch`, so a dry run stays consistent with the
> simulated container world.

### Boot / install

#### `proxy_boot(host, cfg)` / `proxy_boot(host)`

Ensures kamal-proxy is running on `host`. No-op if it is already running
(checked via `sim_container_running`). Otherwise: pulls the image, removes any
stale proxy container (`sim_docker_remove`), and starts a fresh one
(`sim_docker_run`). Throws if the start fails.

`cfg` keys `http_port` (default `80`) and `https_port` (default `443`) are
**documented but reserved** — the proxy uses host networking, so these values
are not currently wired into the run command.

```rhai
proxy::proxy_boot(host);
```

#### `proxy_boot_all(hosts, cfg)` / `proxy_boot_all(hosts)`

Boots the proxy on each host sequentially.

### Deploy (zero-downtime traffic switch)

#### `proxy_deploy(host, service, target, cfg)` / `proxy_deploy(host, service, target)`

Switches traffic for `service` to `target` (a `host:port` string), draining old
connections. Built as
`<cmd> exec kamal-proxy kamal-proxy deploy <service> --target <target> ...` and
routed through `sim_proxy_switch`. Returns the `ExecResult`; throws on failure.

`cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `health_path` | `"/up"` | Adds `--health-check-path <path>` when non-empty. |
| `buffer_requests` | `true` | When true, adds `--buffer-requests` and `--buffer-timeout <n>s`. |
| `buffer_timeout` | `30` | Buffer timeout in seconds. |

```rhai
proxy::proxy_deploy(host, "app", "localhost:3000", #{
    health_path: "/up",
    buffer_requests: true,
    buffer_timeout: 30,
});
```

### Service management

#### `proxy_remove(host, service)`

Removes `service` from kamal-proxy routing (`kamal-proxy remove <service>` via
plain `ssh_exec`).

#### `proxy_set_tls(host, service, domain)`

Enables automatic TLS (Let's Encrypt) for `service` on `domain`:
`kamal-proxy deploy <service> --host <domain> --tls` via plain `ssh_exec`.

#### `proxy_list(host)`

Lists registered services (`kamal-proxy list`). Informational; plain `ssh_exec`.

#### `proxy_stop(host)`

Stops **and** removes the kamal-proxy container (both steps sim-routed via
`sim_docker_stop` then `sim_docker_remove`).

### Maintenance mode

#### `proxy_maintenance(host, service, on_off, cfg)` / `proxy_maintenance(host, service, on_off)`

Puts `service` into (or takes it out of) kamal-proxy's native maintenance page —
every request gets a 503 while in-flight requests drain, **without touching the
registered target**: resuming restores traffic to whatever was already there.

- `on_off == true`: `kamal-proxy stop <service> --drain-timeout=<cfg.drain_timeout>
  [--message <cfg.message>]` (drain default `"30s"`) via plain `ssh_exec`. A non-string
  `drain_timeout` (e.g. a bare int meaning seconds) gets `"s"` appended — `--drain-timeout` is a
  Go duration flag and rejects a unitless number.
- `on_off == false`: `kamal-proxy resume <service>` via plain `ssh_exec`.

```rhai
proxy::proxy_maintenance(host, "app", true, #{ drain_timeout: "10s", message: "Upgrading DB" });
proxy::proxy_maintenance(host, "app", false);                            // back to normal
```

---

## `lib/healthcheck` — Readiness polling

```rhai
import "lib/healthcheck" as health;
```

Retry loops for verifying a service is up after deploy. Three probe styles:
HTTP endpoint, TCP port, and container HEALTHCHECK status.

> **Dry-run note:** `sim_http_healthy` (used by `wait_healthy`) short-circuits
> to a synthetic `200`, and the other `sim_*` probes read the overlay, so a
> loop passes without ever really polling and `sleep` is skipped — `http_get`
> itself is a real, honest probe even under dry-run (issue #16) and is NOT
> part of this short-circuit. With the default `consecutive: 1`, a passing
> loop returns on the first iteration; with `consecutive: N > 1` it still
> takes exactly `N` synthetic iterations to return (each recorded as an
> `[assumed healthy]` line in the dry-run plan), since dry-run only fakes the
> probe's answer, not `wait_healthy`'s own consecutive-pass bookkeeping.

### HTTP health check

#### `wait_healthy(url, cfg)` / `wait_healthy(url)`

Polls `url` until it returns the expected status `consecutive` times **in a
row** (any non-matching response resets the streak). Returns the last
successful `HttpResponse`; throws after exhausting attempts
(`"Health check failed after N attempts: <url> (last status: ..., needed N
consecutive pass(es))"`).

`cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `attempts` | `30` | Max poll attempts. |
| `interval` | `2` | Seconds to `sleep` between attempts. |
| `expected_status` | `200` | Status code that counts as healthy. |
| `consecutive` | `1` | Consecutive passing checks required before returning healthy (robustness review R12) — a single 200 during a flapping boot no longer counts as healthy on its own. |
| `timeout` | `30` | Per-request HTTP timeout in seconds (robustness review R12) — bound this to something small relative to `interval` if a hanging (not erroring, just never responding) endpoint shouldn't be able to make the whole retry loop take up to `attempts * timeout`. |

> **`attempts` is still the hard cap, independent of `consecutive`.** Raising
> `consecutive` does NOT raise the total time budget — worst case is still
> bounded by roughly `attempts * (interval + timeout)`, same as before R12. What
> changes is that a genuinely FLAPPING endpoint can now burn through every
> attempt without ever stringing together `consecutive` passes in a row, and
> throw `"...needed N consecutive pass(es)"` instead of ever returning healthy —
> raise `attempts` too if you raise `consecutive` against a flaky endpoint.

```rhai
health::wait_healthy("http://10.0.0.1:3000/up", #{ attempts: 60, interval: 1, consecutive: 3 });
```

#### `wait_healthy_on_host(host, port, cfg)` / `wait_healthy_on_host(host, port)`

Like `wait_healthy`, but probes `host` **itself over SSH** — it runs `curl` on
`host` against its own `http://localhost:<port><path>`, instead of GETting from
the control machine (robustness review R7-health). Use this (not `wait_healthy`)
to check a deploy target host: the SSH host string is often not a valid HTTP
authority (a `user@host` alias has userinfo, and a plain hostname is commonly
firewalled to just SSH from the control network), so a control-machine GET
against it can fail even when the container is perfectly healthy. `deploy()`'s
own per-host health gate uses this. Only SSH connectivity is required — the
same requirement every other remote operation in this stdlib already has.

Same `cfg` keys as `wait_healthy` (`attempts`, `interval`, `expected_status`,
`consecutive`, `timeout`), plus `path` (default `"/up"`, the path appended to
the probed URL). Returns `#{ status: <int> }`; throws after exhausting attempts
(`"Health check failed on <host> after N attempts: http://localhost:<port><path>
(last status: ..., needed N consecutive pass(es))"`). A transport-level SSH
failure is treated as status `0`, same convention `http_get` uses for its own
transport failures.

```rhai
health::wait_healthy_on_host("deploy@web1", 3000, #{ path: "/up", consecutive: 2 });
```

### TCP port check

#### `wait_port(host, port, cfg)` / `wait_port(host, port) -> bool`

Waits for a TCP `port` to be open on `host`, reading `sim_wait_port`. Returns
`true` on success; throws after exhaustion. `cfg` keys `attempts` (30) and
`interval` (2).

### Container health check

#### `wait_container_healthy(host, name, cfg)` / `wait_container_healthy(host, name) -> bool`

Waits for a container's Docker `HEALTHCHECK` to report `"healthy"`, reading
`sim_container_healthy`. Returns `true`; throws after exhaustion. Requires the
image to define a `HEALTHCHECK` instruction. `cfg` keys `attempts` (30) and
`interval` (2).

### Multi-host HTTP check

#### `wait_healthy_all(hosts, port, cfg)` / `wait_healthy_all(hosts, port)`

Runs `wait_healthy_on_host` against each host in `hosts` **sequentially** (over
SSH, against each host's own `localhost:<port>` — robustness review R7-health;
this used to GET `http://<host>:<port><path>` from the control machine, the same
bug `wait_healthy_on_host` exists to avoid). `cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | `"/up"` | Path appended to each host's probed URL. |
| `attempts` | `30` | Passed through to `wait_healthy_on_host`. |
| `interval` | `2` | Passed through to `wait_healthy_on_host`. |
| `expected_status` | `200` | Passed through to `wait_healthy_on_host`. |
| `consecutive` | `1` | Passed through to `wait_healthy_on_host`. |
| `timeout` | `30` | Passed through to `wait_healthy_on_host`. |

```rhai
health::wait_healthy_all(["10.0.0.1", "10.0.0.2"], "3000", #{ path: "/up" });
```

---

## `lib/registry` — Registry authentication

```rhai
import "lib/registry" as registry;
```

Logs into container registries (Docker Hub, GHCR, generic) on local or remote
hosts, plus an AWS ECR helper. Uses the runtime command from `lib/runtime`.

### `registry_login(host, server, username, password)`

Logs into `server` as `username`. Returns the `ExecResult`; throws on failure.

- `host` — SSH host, or the literal `"local"` for the build machine.
- `server` — registry URL, e.g. `"ghcr.io"`.
- `username` — registry username.
- `password` — a **`Secret`** obtained from `secret("...")`.

**Secret handling — the password stays off-argv.** The login command is built
as:

```
<cmd> login <server> -u <username> --password-stdin
```

Note `--password-stdin`: the plaintext is **never** on the command line, never
in the recorded dry-run plan. The `Secret` is `reveal()`-ed only at the very
last moment and handed to the stdin-capable built-in — `local_exec_stdin` when
`host == "local"`, otherwise `ssh_exec_stdin`. **Never** concatenate a `Secret`
into a string (that throws) and never pipe it via `echo '<pw>' | ...` onto the
command line.

```rhai
import "lib/registry" as registry;

let pw = secret("REGISTRY_PASSWORD");   // reads env var into a Secret
registry::registry_login(host, "ghcr.io", "myuser", pw);
registry::registry_login("local", "ghcr.io", "myuser", pw);
```

### `registry_login_all(hosts, server, username, password)`

Logs into `server` on each host in `hosts`, passing the `Secret` through
unchanged (never stringified) to each `registry_login`.

### AWS ECR

#### `ecr_login(host, cfg)` / `ecr_login(host)`

Logs into AWS ECR using the AWS CLI on the target host. Requires `aws` to be
installed there. Returns the `ExecResult`; throws on failure.

`cfg` keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `region` | `"us-east-1"` | ECR region. |
| `account_id` | `""` | ECR account id. When empty, the account is auto-detected on the host via `aws sts get-caller-identity`. |

The ECR token is produced and consumed **entirely inside the remote shell
pipeline**:

```
aws ecr get-login-password --region <region> | <cmd> login --username AWS --password-stdin <registry>
```

No Rhai-side `Secret` is involved here — the token never enters the script — so
this is a single command string run via `local_exec` / `ssh_exec` (not the
`_stdin` variant). With `account_id` empty, the registry host resolves to
`$(aws sts get-caller-identity --query Account --output text).dkr.ecr.<region>.amazonaws.com`
inside the same subshell.

```rhai
registry::ecr_login(host, #{ region: "eu-west-1", account_id: "123456789012" });
// or auto-detect the account:
registry::ecr_login(host, #{ region: "eu-west-1" });
```
