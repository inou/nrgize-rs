---
title: Fleet-Atomic Deploy
nav_order: 6
---

# Fleet-atomic deploy

`nrg`'s headline feature is a **fleet-atomic, zero-downtime deploy**: roll a new
image across a fleet of hosts behind a health-gated proxy cutover, and if *any*
host fails mid-roll, unwind the *entire* fleet back to the old version. The
fleet is never left half-deployed.

This lives in [`lib/deploy.rhai`](https://github.com/inou/nrgize-rs/blob/main/lib/deploy.rhai) (orchestration) and
[`lib/proxy.rhai`](https://github.com/inou/nrgize-rs/blob/main/lib/proxy.rhai) (the kamal-proxy traffic switch). It builds
on `lib/docker.rhai`, `lib/healthcheck.rhai`, and `lib/runtime.rhai`.

```rhai
import "lib/runtime" as rt;
import "lib/deploy" as deploy;

rt::set_runtime("auto");

deploy::deploy(
    ["deploy@10.0.0.1", "deploy@10.0.0.2"],
    "ghcr.io/org/app:v42",
    "app",
    #{ container_port: 3000, health_path: "/up" },
);
```

Run it with `nrg exec` (live) or `nrg exec --dry-run` (plan only — see
[Dry-run behavior](#dry-run-behavior)).

> Imports in Rhai are **per-file** — they are not inherited from the file that
> imports this module. `import "lib/x" as x;` statements must sit at the **top
> level** of each file. `deploy.rhai` imports everything it touches directly;
> your `Energize.rhai` does the same.

---

## `deploy(hosts, image, service, cfg)`

```rhai
fn deploy(hosts, image, service, cfg)   // full form
fn deploy(hosts, image, service)        // 3-arg overload — cfg defaults to #{}
```

| Arg | Type | Meaning |
| --- | --- | --- |
| `hosts` | array of strings | SSH targets, e.g. `["deploy@10.0.0.1", "web2"]`. Rolled **sequentially**. |
| `image` | string | Full image ref, e.g. `"ghcr.io/org/app:v42"`. The tag becomes the version. |
| `service` | string | Logical service name. Used for container names, proxy routing, and state keys. |
| `cfg` | map (`#{}`) | Options below. Every key is optional; omit `cfg` entirely for all defaults. |

The **version** is derived from the image: it's the last `:`-segment of the
ref (`v42` for `ghcr.io/org/app:v42`), or `"latest"` if the ref has no tag.

### Every `cfg` key

| Key | Default | Effect |
| --- | --- | --- |
| `container_port` | `3000` | The port the app listens on **inside** the container. The host-side port is auto-picked per host (see below). |
| `envs` | `#{}` | Environment variables for the container, e.g. `#{ "RAILS_ENV": "production" }`. Written to a **0600 remote env-file** delivered off-argv and passed via `--env-file` (never `-e KEY=VALUE` on the argv, where they'd be visible to `ps` / `docker inspect`). |
| `volumes` | `#{}` | Volume mounts as `#{ host_path: container_path }`, each becomes `-v host:container`. |
| `health_path` | `"/up"` | HTTP path polled on the new container's host port before the cutover. Also threaded into the proxy via the shared `proxy_cfg` (kamal `--health-check-path`; Caddy active health check). |
| `health_attempts` | `30` | Max health-poll attempts per host before failing the deploy. |
| `health_interval` | `2` | Seconds slept between health attempts (skipped under dry-run). |
| `health_consecutive` | `1` | Consecutive passing checks required before the new container counts as healthy (robustness review R12) — a single 200 during a flapping boot no longer switches traffic to it on its own. |
| `health_timeout` | `30` | Per-request HTTP timeout in seconds for each health check (robustness review R12) — was previously a fixed 30s unrelated to `health_interval`, so a hanging endpoint could make `health_attempts: 30` take up to 15 minutes instead of the intended ~1 minute. |
| `build_context` | `"."` | Docker build context directory. |
| `dockerfile` | `"Dockerfile"` | Dockerfile path passed via `-f`. |
| `build_args` | `#{}` | Build args, each becomes `--build-arg KEY=VALUE`. |
| `platform` | `""` | A single target platform (e.g. `"linux/amd64"`) other than the build machine's own, or a comma-separated list (e.g. `"linux/amd64,linux/arm64"`) for a multi-platform manifest-list build — see [Multi-arch builds](#multi-arch-builds). |
| `build_host` | `""` | Run the build (and, if not skipped, the push) on THIS host over SSH instead of the local machine — see [Multi-arch builds](#multi-arch-builds). |
| `skip_build` | `false` | When `true`, skip the local `build` phase. |
| `skip_push` | `false` | When `true`, skip the registry `push` phase. **Not honored** for a comma-separated `platform`: `buildx build --push` already writes the manifest list to the registry during the build itself, and `deploy()` refuses `skip_push: true` combined with a comma-separated `platform` (when a build will actually run) rather than silently ignoring it — see [Multi-arch builds](#multi-arch-builds). |
| `network` | `""` | Docker network for the container (`--network <name>`). Empty means no extra `--network` flag. |
| `pre_deploy` | `""` | An **in-container** release command (e.g. `"bin/rails db:migrate"`) run **once for the fleet** in a throwaway container built on the **new** image (`docker run --rm <image> <pre_deploy>`), with the same `envs`, BEFORE any traffic switches. A non-zero exit **throws** and aborts the deploy. This is the correct place for migrations. |
| `pre_deploy_cmd` | `""` | Legacy: a raw shell command run on **each host via SSH** before that host's new container starts (inside the transaction). Use `pre_deploy` for anything that must run against the new image's code. |
| `post_deploy_cmd` | `""` | Shell command run on each host via SSH **after the whole fleet is committed**. Best-effort: it never throws (nothing after commit can be rolled back), but a failed host is now printed loudly as a `[warn]` naming exactly which host(s) failed and why (robustness review R20) — it no longer reports full success on a partial failure. |
| `proxy` | `"kamal"` | Proxy backend: `"kamal"` (`lib/proxy.rhai`) or `"caddy"` (`lib/caddy.rhai`). See [Choosing the proxy](#why-kamal-proxy-and-swapping-proxies). |
| `domain` | `""` | Service domain. With `proxy: "caddy"`, adds a host match so Caddy's automatic HTTPS issues a Let's Encrypt certificate. |
| `keep_images` | unset | Tagged-image retention (robustness review R22), **strictly opt-in**. `docker_cleanup`'s prune only ever removed *dangling* (untagged) images — `image_repo`'s own old tagged versions (`myapp:v41`, `myapp:v40`, ...) accumulated on every host forever otherwise. Set `keep_images: N` (`N >= 0`) to prune each host's other tags of `image_repo` beyond the `N` most recently created, run best-effort right after that host's post-commit cleanup (a listing/prune failure is a `[warn]`, never a thrown error, and never blocks persisting the new port/target). The tag just deployed, and — if it's the same repo — the previous version `rollback()` might still need, are **always** protected regardless of age. A negative value throws (an explicit `-1` is a caller mistake, not "disabled"; omit the key entirely to disable). Persisted into the replayed `<service>.config` only when the caller actually sets it, so a later `rollback()` keeps the same pruning behavior. |
| `skip_lock` | `false` | Opt out of the cross-machine deploy lock (robustness review R15) — see [Cross-machine deploy lock](safety.md#cross-machine-deploy-lock-robustness-review-r15). On by default; only skip it if you have some other reason to be certain no concurrent deploy of this service can happen. |

> **Migrations:** use `pre_deploy` (NOT `pre_deploy_cmd`). It runs your release command
> **once**, in a fresh container built on the **new** image, before any host switches traffic.
> A failure aborts the deploy. The old pattern of `exec`'ing `rails db:migrate` into the
> still-running **old** container (guarded with `|| true`) was wrong: it ran the new
> migrations against the old image and swallowed failures. The example `Energize.rhai` now uses
> `pre_deploy: "bin/rails db:migrate"`.
>
> `deploy()` persists the full effective `cfg` under `<service>.config`, so `rollback()`
> replays the exact same envs/port/health/proxy instead of reverting to defaults.

`deploy()` **throws** if any host fails. The whole fleet unwinds atomically
*before* the error is re-raised, so you can catch it or let it abort the run.

---

## The lifecycle

A deploy runs in two parts: a **preamble outside the transaction**, then **one
transaction** wrapping the rolling loop, then a **post-commit cleanup**.

### Outside the transaction (no rollback for these)

These phases run before any running container is touched, so they need no
compensation:

1. **Build** (`skip_build` to skip) — `docker build -t <image> -f <dockerfile>
   <build_args> <context>`, run locally. If `cfg.platform == ""` (the default), this is
   preceded by an architecture preflight: this machine's and `hosts[0]`'s `uname -m` are
   compared (LIVE runs only — see [Multi-arch builds](#multi-arch-builds)) and a mismatch
   **throws** before any build/push/pull time is spent, instead of surfacing later as an
   opaque exec-format error when the container fails to start.
2. **Push** (`skip_push` to skip) — `docker push <image>`, run locally.
3. **Pull on all hosts** — `docker pull <image>` fanned out to **all hosts in
   parallel** (`ssh_exec_all`). Fan-out is safe here: a pull failure aborts
   *before* the rolling loop touches anything live. A failed pull on any host
   throws and names the failed hosts.
4. **Proxy boot** — `proxy::proxy_boot(host)` on each host ensures the
   `kamal-proxy` container is up (no-op if it already is). See
   [Why kamal-proxy](#why-kamal-proxy-and-swapping-proxies).
5. **Snapshot the previous image** — if `<service>.image` is already in state
   and differs from the new image, it's copied to `<service>.prev` so
   [`rollback()`](#rollbackhosts-service-cfg) has a target. Done here (not inside
   the transaction) so the snapshot survives a successful commit.

### Inside one transaction — the rolling loop

The entire fleet rolls inside a single `transaction(|| { ... })`. Hosts are
processed **sequentially** with per-host SSH calls (never `ssh_exec_all`):
fan-out would swallow per-host failures, defeating the atomic unwind.

For each host, `deploy_one_host` does:

1. **Capture the OLD proxy target by value** — read `<service>.target.<host>`
   (or fall back to the recorded port, then `localhost:<container_port>`). The
   old container keeps running on its old host port, so this value is exactly
   what a rollback restores.
2. **Pick a fresh host port** — `sim_pick_port(host, container_port)` finds a
   free port (live: `nc -z` scan upward from `container_port + 10000`; dry-run:
   a deterministic symbolic port). That free port also yields a **collision-proof
   unique container name** `<service>-web-<version>-<port>`, so the new container
   can coexist with the still-running old one.
3. **Start the NEW container** under that unique name, publishing
   `<picked_port>:<container_port>`, with the configured `envs`/`volumes`/`network`.
4. **Register the rm-new compensation immediately** — `on_rollback(|| rm -f
   <new_name>)` is registered *right after* the container starts, **before** the
   health wait. This is deliberate: a health-check failure is the most common
   failure mode, and registering the inverse before the effect guarantees the
   new container is torn down on unwind. (`rm -f ... || true` is idempotent.)
5. **Health-check the new container** — `health::wait_healthy_on_host(host, picked_port,
   #{ path: health_path, ... })` runs `curl` **on `host` itself** (over SSH, against its
   own `localhost:<picked_port>`) until it sees HTTP 200 (or fails after `health_attempts`).
   Deliberately host-side, not a control-machine GET: the SSH host string is often not a
   valid HTTP authority (a `user@host` alias has userinfo, and the ephemeral port is
   commonly firewalled from the control network) — see robustness review R7-health. This
   is an **HTTP** check only — it does *not* require a Docker `HEALTHCHECK` instruction,
   which many images don't define.
6. **Register the restore-proxy compensation BEFORE switching** —
   `on_rollback(|| proxy::proxy_deploy(host, service, OLD_target))`. Registered
   before the cutover so, on unwind, traffic flows back to the still-running old
   container *first* (no blackhole window), then the new container is removed.
7. **Switch the proxy** — `proxy::proxy_deploy(host, service, "localhost:<picked_port>", #{ health_path })`
   runs `kamal-proxy deploy <service> --target <target> --health-check-path
   <path>`, draining old connections.
8. **The OLD container is LEFT RUNNING** under its canonical name
   `<service>-web`. It is *not* stopped or removed inside the window. That's what
   makes the rollback path a simple proxy flip back to a container that's still
   alive.

`deploy_one_host` returns `#{ host, new_name, new_port }` so the post-commit
pass can finish the swap.

### Post-commit (transaction returned Ok)

Only after the **whole fleet** has switched does cleanup run — one pass across
the fleet. For each rolled host:

1. Rename the old `<service>-web` to `<service>-web-old`.
2. Rename the new container to the canonical `<service>-web`.
3. Stop and remove `<service>-web-old`.
4. Prune exited containers and dangling images.
5. **Persist the port + target now** (never inside the transaction):
   `<service>.port.<host>` and `<service>.target.<host>`. Persisting only
   post-commit means a mid-fleet failure can't leave a stale port that would
   corrupt the *next* deploy's captured old-target.

Then the best-effort `post_deploy_cmd` runs per host, and the final state is
written: `<service>.version`, `<service>.image`, `<service>.deployed_at`.

All cleanup mutations use `rm -f`/`|| true` and are idempotent, so a partial
cleanup never wedges a re-run.

---

## Why it's fleet-atomic

The key invariant: **the old container is never destroyed inside the deploy
window.** It keeps running under its canonical name the whole time the new one
is being started, health-checked, and cut over to. Old-container retirement
happens only *after* every host has successfully committed.

Because the whole rolling loop lives inside **one** `transaction`, a failure on
host N (a failed start, a failed health check, a failed proxy switch, or a
failed `pre_deploy_cmd`) throws, and the transaction runtime drains the
registered compensations **LIFO, best-effort, error-isolated**, then re-raises
the original error.

Consider a 3-host fleet where host 3's health check fails. By that point hosts 1
and 2 have switched and registered two compensations each (restore-proxy, then
rm-new), and host 3 has registered its rm-new. The unwind pops them in reverse
registration order:

```
host 3: rm -f new container         (proxy was never switched on host 3)
host 2: restore proxy -> OLD target (traffic back to still-running old container)
host 2: rm -f new container
host 1: restore proxy -> OLD target
host 1: rm -f new container
```

For each already-switched host the proxy is restored to the old target **before**
the new container is removed — so there's no moment where traffic points at a
removed container. The end state: every host is back on the old version, serving
from the old container that was never stopped. Then the original error
propagates.

A compensation that itself throws is logged (`[nrg] rollback step failed
(continuing)`) and the unwind continues — one broken cleanup step can't strand
the rest of the fleet half-rolled.

> **Sequential, not parallel.** Hosts roll one at a time. This is intentional:
> `ssh_exec_all` fan-out swallows per-host failures, which would break the atomic
> unwind. The trade-off is that a large fleet takes longer to roll. The window in
> which different hosts run different versions is the duration of the roll —
> acceptable for the rolling-update model, but not a globally-atomic flip.

---

## Multi-arch builds

The most common cross-arch setup is building on an Apple Silicon laptop and
deploying to an x86 VPS. A plain `docker build` bakes in the **local**
machine's architecture; without something catching that, the mismatch doesn't
surface until the new container fails to start on the host with an opaque
exec-format error — *after* build, push, and pull have all already appeared to
succeed.

Two things guard against this, both driven by `cfg.platform`:

1. **`cfg.platform`** (default `""`) — one or more target platforms.
   - A **single** platform, e.g. `"linux/amd64"`: `docker_build` uses
     `buildx build --platform <value> --load` instead of a plain `build`.
     `--load` keeps the result a normal local image, so the existing separate
     `docker_push` step still works unchanged.
   - A **comma-separated list**, e.g. `"linux/amd64,linux/arm64"`: this builds
     a genuine multi-platform **manifest list** instead. `buildx` can't
     `--load` more than one platform into the local image store, so this uses
     `--push` and publishes the manifest list straight to the registry during
     the build. `deploy()` detects the same comma and automatically skips its
     own separate `docker_push` step, since there's nothing local left to
     push (an informational message notes the skip). Because the registry
     write happens as part of the build, `cfg.skip_push` can't be honored for
     a comma-separated `platform` — `deploy()` **throws** up front if you set
     `skip_push: true` together with a comma-separated `platform` (and a
     build will actually run), rather than silently ignoring your request.
2. **The arch preflight** — when `cfg.platform == ""` (i.e. you haven't
   already told `deploy()` which architecture to target), `deploy()` compares
   the BUILD machine's `uname -m` (this machine, or `cfg.build_host` if set —
   see below) against `hosts[0]`'s (normalizing e.g. macOS's `arm64` and
   Linux's `aarch64` to the same value first, so an ARM laptop deploying to an
   ARM VPS is correctly recognized as a match) and **throws** a clear,
   actionable error if they differ, before any build/push/pull runs.

```rhai
deploy::deploy(WEB_HOSTS, "ghcr.io/org/app:v42", "app", #{
    platform: "linux/amd64",   // building on an ARM laptop, deploying to an x86 fleet
});
```

3. **`cfg.build_host`** (default `""`, roadmap 1.1 step 3a) — run the build on
   a DIFFERENT machine over SSH instead of locally. The common case: a native
   arm64 builder box, so an arm64 target needs no buildx/qemu emulation at
   all — just build natively on a matching-arch machine and push straight
   from there. Composes with `cfg.platform`: it's simply WHERE the same build
   command (plain `build` or `buildx build`, single- or multi-platform) runs.

   ```rhai
   deploy::deploy(WEB_HOSTS, "ghcr.io/org/app:v42", "app", #{
       build_host: "deploy@arm-builder",   // build+push happen there, not locally
   });
   ```

   The build (and the registry push) run ON `build_host` — if you're using
   `recipe::standard_deploy`, its registry-login step now also logs in on
   `build_host` when it's set (in addition to `"local"` and `web_hosts`).
   Calling `deploy()` directly instead? Log in on `build_host` yourself
   first (`registry_login(build_host, ...)`), same as you already do for
   `web_hosts` — otherwise a private base image pull during the build, or
   the push afterward, fails live with an "unauthorized" error.

   `context` is synced to `build_host` first. This codebase has no existing
   context-sync primitive (no `rsync`/`scp` is ever invoked by any stdlib
   function — `nrg doctor` only checks for them on `PATH`), so this builds one
   from the EXISTING `local_exec`/`ssh_exec_stdin` primitives: `tar` the
   context locally, pipe through `base64`, ship the result as a string, decode
   and extract remotely. base64 isn't cosmetic — command output/input round-
   trips through Rust `String`s internally, which would silently corrupt raw
   binary tar bytes; base64 keeps every byte ASCII-safe. The whole archive is
   buffered in memory on both ends (no streaming), so keep `context` small —
   a present `context/.dockerignore` is honored as a `tar --exclude-from`
   (best-effort: tar's exclude-glob syntax isn't a perfect match for
   `.dockerignore`'s, but covers the common case). **Limitation:** only
   `context` is synced, so `dockerfile` must resolve to a path INSIDE it (the
   default, `"Dockerfile"`, always does) — a `dockerfile` pointing outside
   `context` won't be found on `build_host`. If you push afterwards
   (`!cfg.skip_push`), `deploy()` pushes from `build_host` too, not locally —
   the image only exists there.

**LIVE runs only.** `local_exec` is a MUTATING-class builtin, so it's stubbed
under `--dry-run` (see [Dry-run behavior](#dry-run-behavior)) and can't read
this machine's real architecture in a plan — comparing a real remote probe
against a fake local value would be worse than not checking at all, so the
preflight is skipped entirely under `--dry-run` (a printed note says so). This
is a live-run-only safeguard, in the same spirit as the rest of the live
deploy path's test-coverage limits (see
[Robustness Review](robustness-review.md)).

---

## `rollback(hosts, service, cfg)`

```rhai
fn rollback(hosts, service, cfg)   // cfg: #{ image: "" }
fn rollback(hosts, service)        // 2-arg overload — use the snapshotted prev
```

Rolls the fleet back to a previous image. With no explicit `cfg.image`, it uses
the `<service>.prev` snapshot written by the last `deploy()`. If neither is
present it **throws** (`No rollback image found for <service>...`).

```rhai
import "lib/deploy" as deploy;

// Roll back to the snapshotted previous image:
deploy::rollback(["deploy@10.0.0.1", "deploy@10.0.0.2"], "app");

// Or to a specific image:
deploy::rollback(hosts, "app", #{ image: "ghcr.io/org/app:v41" });
```

Mechanically, `rollback()` is just `deploy()` with the rollback image and
`skip_build: true, skip_push: true` — so it reuses the exact same fleet-atomic
path. Before overwriting, it saves the *current* image as the next
`<service>.prev`, so you can roll back and forth.

> The `.prev` snapshot holds **one** prior image, not a full history. After a
> rollback, `.prev` points at the image you just rolled *off of*. There's no
> deeper undo stack — pass `cfg.image` explicitly to jump to an older tag.

---

## Lifecycle hooks

`deploy()`/`rollback()` will call back into three OPTIONAL functions your
orchestration file may (but need not) define, by exact name and arity:

```rhai
fn hook_pre_deploy(service, image, hosts)    { ... }  // may THROW to block the deploy
fn hook_post_deploy(service, image, hosts)   { ... }  // best-effort
fn hook_post_rollback(service, image, hosts) { ... }  // best-effort
```

These are unrelated to `cfg.pre_deploy` (an in-container release command run
against the new image) and `cfg.pre_deploy_cmd`/`cfg.post_deploy_cmd`
(trusted-input-only raw host shell) — deliberately named `hook_*` to avoid any
confusion with those existing cfg keys.

- **`hook_pre_deploy`** runs first, before any build/push/pull/health-check
  work — it can genuinely **block** the deploy by throwing, aborting cleanly
  with no image/container work done. (The cross-machine deploy lock, R15, is
  already held by this point — a throw here still releases it correctly,
  same as any other early failure in `deploy()`.) It also fires during a
  `rollback()` — called by `rollback()` itself, *before* it overwrites its
  own `.prev` snapshot, precisely so a block never corrupts the rollback
  chain (a caller who hits the block and retries later must still roll back
  to the *original* target, not the broken image the rollback never
  finished switching away from).
- **`hook_post_deploy`** runs after the fleet has already committed and the
  lock has been released. It's **best-effort**: if it throws, the failure is
  printed as a warning, but the deploy is still reported as a success — the
  same convention `cfg.post_deploy_cmd` already follows.
- **`hook_post_rollback`** runs after a successful `rollback()`, in
  **addition to** (not instead of) `hook_post_deploy` — since `rollback()`
  calls `deploy()` internally, a rollback fires both hooks, letting you tell
  a routine deploy apart from a rollback (e.g. post a routine "v42 live"
  message from `hook_post_deploy`, but page someone from
  `hook_post_rollback`). Also best-effort.

A hook is looked up by name **and** arity together — a function named
`hook_post_deploy` with the wrong number of parameters is treated exactly
like not having defined the hook at all, not as an error.

```rhai
import "lib/deploy" as deploy;
import "lib/notify" as notify;

fn hook_post_deploy(service, image, hosts) {
    notify::slack(secret("SLACK_WEBHOOK_URL"), service + " " + image + " live on " + hosts.len() + " host(s)");
}
fn hook_post_rollback(service, image, hosts) {
    notify::slack(secret("SLACK_WEBHOOK_URL"), "ROLLED BACK " + service + " to " + image);
}

deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v42", "app", #{ container_port: 3000 });
```

> **`nrg rollback` fires neither hook.** The native `nrg rollback <service>`
> CLI command (roadmap 3.3) synthesizes its own standalone script — it never
> evaluates your orchestration file at all, so hooks defined there aren't
> visible to it, including `hook_post_deploy` (since the CLI's synthesized
> script calls `rollback()` directly, bypassing your file's `deploy()` call
> entirely too). Hooks only fire when `deploy()`/`rollback()` are called
> **from within your own orchestration file's code** — e.g. a
> project-authored `rollback` task function invoked via `nrg run rollback`,
> or a script that calls `deploy::rollback(...)` directly. If you rely on
> `hook_post_rollback` to page someone, `nrg rollback <service>` will roll
> back silently — use a project-authored rollback task instead if you need
> the hook to fire during an incident.

### `lib/notify.rhai` — generic webhook helper

```rhai
fn webhook(url, payload)   // POST an already-serialized JSON string
fn slack(url, text)        // POST a Slack incoming-webhook {"text": ...} payload
```

A thin wrapper over the `http_post` builtin (already dry-run-safe — it never
executes a real request in `--dry-run`, just records a planned "assumed ok"
check), so a hook doesn't need to hand-write JSON escaping. `slack`
JSON-string-escapes `text` for you, so a message containing quotes or
newlines can never produce malformed JSON.

`url` may be a plain string or a Secret (from `secret("...")`, as in the
example above) — a webhook URL usually functions as a bearer credential, so
treating it like one is deliberate. Both functions reveal a Secret
internally right before the request, so you do not (and should not) call
`reveal()` yourself; the real URL still never appears in a `--dry-run` plan.

---

## `accessory_run(host, name, image, cfg)`

```rhai
fn accessory_run(host, name, image, cfg)   // cfg: #{ ports, envs, volumes, network, cmd }
fn accessory_run(host, name, image)         // 3-arg overload — defaults
```

Starts a long-lived accessory container (database, Redis, etc.) if it isn't
already running. Accessories are **not** part of the rolling deploy — they have
no health-gated cutover and no rollback. The running check is sim-routed
(`docker_container_running`), so it reads honestly under dry-run.

```rhai
deploy::accessory_run("deploy@10.0.0.3", "app-db", "postgres:16", #{
    ports:   #{ "5432": "5432" },
    envs:    #{
        "POSTGRES_DB":       "app_production",
        "POSTGRES_USER":     "app",
        "POSTGRES_PASSWORD": reveal(secret("DB_PASSWORD")),
    },
    volumes: #{ "/var/lib/app-db": "/var/lib/postgresql/data" },
});
```

| `cfg` key | Default | Effect |
| --- | --- | --- |
| `ports` | `#{}` | Port publishes `#{ host: container }`. |
| `envs` | `#{}` | Environment variables. |
| `volumes` | `#{}` | Volume mounts. |
| `network` | `""` | Docker network. |
| `cmd` | `""` | Extra trailing args (appended after the image, via `extra`). |

If the run fails it **throws**. If the container is already running it's a no-op.

If a container by this name exists but is **stopped** (a prior crashed run, or a
manual `docker stop`), `accessory_run` removes it and starts fresh, rather than
failing with Docker's "name already in use" (robustness review R10b). This
discards that container's **writable-layer** data — put anything that needs to
survive a stop/restart in a named volume (`cfg.volumes`, as in the example
above), not the container's own filesystem.

After starting, `accessory_run` also briefly re-checks that the container is
still running (catching a misconfigured accessory that starts and crashes
almost immediately, e.g. a database given the wrong credentials) and **throws**
if it isn't — this is a one-shot liveness check, not a configurable health gate
like the main app's rolling deploy has via `health_path`/`health_attempts`.

---

## `accessory_stop(host, name)` / `accessory_restart(host, name)` / `accessory_upgrade(host, name, image, cfg)`

```rhai
fn accessory_stop(host, name)
fn accessory_restart(host, name)
fn accessory_upgrade(host, name, image, cfg)   // cfg: same shape as accessory_run's
fn accessory_upgrade(host, name, image)         // 3-arg overload — defaults
```

`accessory_run`'s own "already running" check is **by name only** — a running
`myapp-db` container blocks it from ever noticing an image bump, so a
`postgres:16` → `postgres:17` upgrade has no answer from `accessory_run` alone.
These three functions round out a supported stop/restart/upgrade lifecycle for
an accessory once it's running.

```rhai
deploy::accessory_stop("deploy@10.0.0.3", "app-db");
deploy::accessory_restart("deploy@10.0.0.3", "app-db");
deploy::accessory_upgrade("deploy@10.0.0.3", "app-db", "postgres:17", #{
    ports:   #{ "5432": "5432" },
    volumes: #{ "/var/lib/app-db": "/var/lib/postgresql/data" },
});
```

- **`accessory_stop`** stops the container (`docker stop`) but never removes
  it, so its named volumes (and any bind mount) are untouched. It's
  idempotent: stopping an already-stopped (or never-started) accessory is a
  no-op, not an error — matching `docker_stop`'s own `|| true` semantics one
  level up, so a caller never has to check first.

- **`accessory_restart`** restarts the *existing* container in place
  (`docker restart`), reusing whatever image and config it's already running.
  It takes no `image` argument by design: Docker's own `restart` can't change
  what image a container runs, so there's nothing to pass. Useful for picking
  up a config change delivered via a mounted volume, or clearing a stuck
  process, without touching the image. For an image bump, use
  `accessory_upgrade` instead.

- **`accessory_upgrade`** first **pulls the new image** (mirroring `deploy()`'s
  own pull-before-transaction ordering), so a bad or unpushed tag, or a
  registry-auth failure, throws with the OLD container still up. Note this
  only proves the image can be *pulled*, not that it can *run* — a pullable
  image that immediately exits (bad entrypoint, missing required env var,
  architecture mismatch) still throws only after the old container has
  already been stopped and removed, same as `accessory_run`'s own
  immediate-exit check; there is no automatic rollback to the prior image
  (accessories are documented as having none). Only after the pull succeeds
  does it stop and remove the old container, and start `name` fresh on the
  new `image` via `accessory_run` itself — reusing its start-and-verify
  logic. The removal never passes `-v`, so a named volume in `cfg.volumes`
  survives the upgrade untouched (a bind-mounted host path is unaffected
  either way, since removing a container never touches the host filesystem)
  — pass the **same** `cfg` you deployed the old image with, unless the
  upgrade is also changing ports/envs/etc.

All three are sim-routed (`accessory_restart` via a new `docker_restart`
wrapper, alongside `accessory_run`'s existing `docker_stop`/`docker_remove`),
so a `--dry-run` plan reflects the same outcome a live run would produce.
`accessory_restart` and `accessory_upgrade` throw on failure (a failed
`ssh_exec`/`docker_pull`, or anything `accessory_run` itself would throw for
during the upgrade's restart step). `accessory_stop` does not: like
`docker_stop` one level up, a transport failure during the stop itself is
swallowed (`|| true`) — only an unreachable host during the *pre-check probe*
throws, and only in live mode.

---

## State keys

`deploy()` reads and writes these keys in the persistent state store. `state_get`
returns `()` (unit) when a key is absent — test presence with `has_state(k)` or
`state_get(k) != ()`, never `if state_get(k) { ... }` (Rhai conditions must be
`bool`).

| Key | Written | Holds |
| --- | --- | --- |
| `<service>.version` | post-commit | The deployed version (image tag). |
| `<service>.image` | post-commit | The full image ref just deployed. |
| `<service>.prev` | preamble (before the roll) | The previous `<service>.image`, the rollback target. |
| `<service>.port.<host>` | post-commit, per host | The host-side port the live container publishes. |
| `<service>.target.<host>` | post-commit, per host | The proxy target `localhost:<port>` for that host. |
| `<service>.deployed_at` | post-commit | UTC timestamp string. |
| `<service>.config` | post-commit | The full effective `cfg` as JSON, so `rollback()` can replay it. **May contain plaintext secret values** if `cfg.envs` was built from `reveal()`'d secrets — see [Safety Features: "Deploy state may contain secret plaintext"](safety.md#deploy-state-may-contain-secret-plaintext-robustness-review-r24) (robustness review R24). |
| `nrg.runtime.cmd` / `nrg.runtime.name` | post-commit (re-synced every deploy) | The container CLI this deploy actually used (`"docker"`/`"podman"`/`"nerdctl"`, and the human-readable name). This is a durable MIRROR for `nrg status`/`nrg logs`/`nrg app exec` (separate CLI invocations with no Rhai engine of their own) — the *live* choice within a script run lives in the ephemeral, never-persisted `session` store instead (robustness review R27; see [Standard Library: `lib/runtime`](stdlib.md#libruntime--container-runtime-selection)). |

Every key in `.energize/state.json` is only as protected as the file itself:
written **0600** (owner-only), but that's a local-exposure mitigation, not a
guarantee against backups, CI artifact uploads, or workspace archiving — see
the R24 link above.

The per-host `port`/`target` keys are written **only post-commit**, so a
mid-fleet failure can't persist a stale port. On the *next* deploy,
`deploy_one_host` reads `<service>.target.<host>` (then `.port.<host>`) to know
where to send traffic back to on rollback.

> `<service>.deployed_at` comes from `local_exec("date -u ...")`. Because
> `local_exec` is a mutating-class builtin, a `--dry-run` records the action and
> returns synthetic empty stdout — so `deployed_at` is **empty in a plan**. That's
> acceptable (plan-only).

---

## Why kamal-proxy (and swapping proxies)

The deploy model needs one specific capability from the proxy: **a health-gated
atomic cutover for a named service, in a single command.** kamal-proxy provides
exactly that:

```
kamal-proxy deploy <service> --target <host:port> --health-check-path <path>
```

That one call points `<service>`'s traffic at the new target, health-gates it,
and drains in-flight connections off the old target. It maps one-to-one onto
"switch the proxy to the new container" — no config files, no reloads. The
`proxy_deploy()` wrapper in `lib/proxy.rhai` runs it (plus `--buffer-requests`
/ `--buffer-timeout` by default) **inside** the proxy container via
`<runtime> exec kamal-proxy ...`.

### Choosing the proxy: `cfg.proxy`

`nrg` ships **two** proxy modules with the same surface, and `deploy()` selects
between them with `cfg.proxy`:

- **`"kamal"`** (default) — `lib/proxy.rhai`, the one-command cutover above.
- **`"caddy"`** — `lib/caddy.rhai`, which runs Caddy with its admin API on
  `localhost:2019`. The cutover is an atomic admin-API call that replaces the
  service route's upstream (a route tagged `@id:<service>`, create-or-update).
  Passing `cfg.domain` adds a host match, so Caddy's automatic HTTPS issues a
  Let's Encrypt certificate for the domain.

```rhai
// Use Caddy instead of kamal-proxy, with automatic TLS:
deploy::deploy(hosts, image, "app", #{
    proxy:  "caddy",
    domain: "app.example.com",
});
```

`deploy()` is proxy-agnostic: it imports both modules and dispatches every
`proxy_boot` / `proxy_deploy` call (including the rollback compensation) on
`cfg.proxy`, threading `cfg.domain` through. Both modules also expose
`proxy_remove` / `proxy_set_tls` / `proxy_list` / `proxy_stop` /
`proxy_maintenance`.

### Maintenance mode: `proxy_maintenance(host, service, on_off, cfg)`

Puts `service` into (or takes it out of) maintenance mode — every request gets
a 503 while it's on. The two backends implement this differently, since only
kamal-proxy has a native suspend/resume primitive:

- **kamal-proxy**: `kamal-proxy stop <service> --drain-timeout=<cfg.drain_timeout>
  [--message <cfg.message>]` (drain default `"30s"`) suspends the route
  WITHOUT forgetting its target; `kamal-proxy resume <service>` brings it back
  exactly as it was — no extra info needed. `cfg.message` customizes the text
  shown on kamal-proxy's own 503 page (passed straight to its `--message`
  flag); there's no kamal-proxy equivalent of Caddy's `cfg.status_code` (it's
  silently ignored on this backend — kamal-proxy's maintenance response is
  always a 503).
- **Caddy**: has no such primitive, so maintenance mode PATCHes only the
  route's `handle` (via the same `/id/<service>/handle` sub-path trick
  `proxy_set_tls` uses for `/match`) to a static response (`cfg.status_code`,
  default `503`; `cfg.message`, default a generic notice) — leaving the
  route's `match` (host/domain) and everything else untouched. Turning
  maintenance back **off** requires `cfg.target` (the `host:port` to restore)
  — Caddy has no memory of what a route's handler used to point at once it's
  been replaced. Because `match` was never touched, `cfg.domain` does **not**
  need to be re-supplied to restore a domained/TLS service. The route must
  already exist (`proxy_deploy` first) — there's nothing sensible to toggle
  on a service that was never deployed. `match` is preserved automatically,
  but the active health check is NOT — like `proxy_deploy`, it's rebuilt from
  `cfg.health_path` on each call, so pass the same `health_path` you deploy
  with or the restored route comes back without one until the next deploy.

```rhai
proxy::proxy_maintenance(host, "app", true);                              // on (kamal default 30s drain)
proxy::proxy_maintenance(host, "app", false);                             // off (kamal)
proxy::proxy_maintenance(host, "app", false, #{ target: "localhost:13000" }); // off (Caddy — target required)
```

A `nrg run`-able task, defined in your own orchestration file. Put it in a
file whose top level does NOT unconditionally deploy — per [`nrg
run`](cli.md#nrg-run), "the file's top level is evaluated first ... then the
named function is invoked", so a script whose top level directly calls
`recipe::standard_deploy(...)` (like `lib/examples/*.rhai`) would trigger a
full redeploy as a side effect of `nrg run maintenance` too:

```rhai
import "lib/proxy" as proxy;
const WEB_HOSTS = ["deploy@web1.example.com", "deploy@web2.example.com"];

fn maintenance(on) {
    let on_off = on == "true";
    for host in global::WEB_HOSTS {
        proxy::proxy_maintenance(host, "app", on_off);
    }
}
```

```bash
nrg run maintenance true    # nrg run scale 3 calls scale("3") — args are always strings
nrg run maintenance false
```

Why kamal-proxy is the default, and the tradeoffs:

- **nginx** is config-plus-reload: a cutover means rewriting an upstream block
  and `nginx -s reload` — a poor fit for "one step flips one named service
  atomically and drains the old one." You'd template config, manage reload
  races, and implement draining yourself.
- **Caddy** is API-driven: the admin API reconfigures the upstream atomically
  and brings free automatic TLS, at the cost of more moving parts than the
  single kamal-proxy command. `lib/caddy.rhai` wraps that for you.

#### Adding your own proxy

The proxy is a thin, swappable surface. To add another (say Traefik), write a
`lib/traefik.rhai` exposing the **same functions** `deploy.rhai` dispatches to:

```rhai
fn proxy_boot(host)                          { /* ensure the proxy is up */ }
fn proxy_deploy(host, service, target, cfg)  { /* health-gated cutover to target */ }
fn proxy_remove(host, service)               { /* drop service from routing */ }
```

then add a branch to `deploy.rhai`'s `px_*` dispatch (or call your module from
your own deploy wrapper). Match those signatures — and the **health-gated,
drain-on-switch** semantics — and the fleet-atomic machinery works unchanged.
The hard part of any replacement is reproducing the atomic, health-checked,
draining cutover; if your proxy can't do that in one step, you take on the
orchestration yourself.

> For dry-run fidelity, the running check and traffic switch route through the
> typed sim builtins (`sim_container_running`, `sim_proxy_switch`). A real proxy
> module should do the same (route mutations/reads through the `sim_*` builtins)
> so plans stay consistent — see the next section.

---

## Dry-run behavior

`nrg exec --dry-run` produces a plan of side effects without executing them: it
takes no state lock, writes no state to disk (it uses an in-memory overlay), and
runs no SSH or local commands for real. Concretely, in dry-run:

- **Mutating builtins record** the action they *would* run and return a
  synthetic success — `local_exec`, `ssh_exec`, and the container mutations
  (`sim_docker_run`/`stop`/`rename`/`remove`, `sim_proxy_switch`) record an entry
  and return synthetic `ok`. `state_set` records and writes only to the overlay.
- **Reads go through the sim/overlay** — `sim_container_running`,
  `sim_container_healthy`, `sim_image_id`, `sim_wait_port`, and `state_get` answer
  from the simulated container world and overlay, so a dry plan stays
  *internally consistent*: a container `sim_docker_run` "started" reads back as
  running and healthy, and `state_get` sees overlay writes.
- **`http_get` short-circuits** to a synthetic `200`, so a `wait_healthy` loop
  against a not-yet-started container neither fails nor hangs the plan. (Its poll
  loop therefore never really iterates under dry-run.) `wait_healthy_on_host` (the
  SSH-based check `deploy()` actually uses — see R7-health) short-circuits the same
  way via `is_dry_run()`, with no `ssh_exec` call at all under dry-run.
- **`sleep` is skipped** entirely (the `health_interval` waits cost nothing in a
  plan).
- **`sim_pick_port` is deterministic** in dry-run (a symbolic port,
  `container_port + 10000` incrementing per pick) — no real `nc -z` probe. The
  new container name uses `<auto>` for the port label in printed output.
- **`on_rollback` records** the registration but the compensation is **never
  invoked** in dry-run; the rolling loop's transaction is exercised for its plan,
  not its unwind.

So a `--dry-run` deploy walks the entire lifecycle and emits the full sequence
of commands it would run, with a self-consistent simulated fleet, but mutates
nothing.

---

## Secrets and Rhai gotchas

A few things to keep correct when wiring up a deploy:

- **Secrets can't be concatenated _or interpolated_.** `secret("X")` returns a
  `Secret` type that refuses string concatenation (`"x" + secret` throws) AND
  string interpolation (`` `... ${secret} ...` `` is rejected at the command
  boundary), to keep plaintext out of traces/argv. To put a secret value into an
  `envs` map, wrap it: `reveal(secret("SECRET_KEY_BASE"))` yields the plaintext
  (still redacted in plan output, and the `envs` map is delivered to the
  container via a 0600 `--env-file`, off-argv). For a shell argument use
  `sh_quote(secret)`. Pass the *raw* `Secret` to `registry_login`, which streams
  it to `--password-stdin` off-argv. A password going into a URL should also be
  `url_encode()`'d.

  ```rhai
  deploy::deploy(hosts, image, "app", #{
      envs: #{
          "SECRET_KEY_BASE": reveal(secret("SECRET_KEY_BASE")),
          // building a DATABASE_URL: reveal once into a let, then concatenate the string
      },
  });
  ```

- **`#{}` config maps, no kwargs.** Rhai has no keyword arguments — pass one
  config map. Use `fail`'s equivalent: `throw "message"` to abort.
- **`trim()` mutates** in place and returns unit; the `timestamp()` helper calls
  `out.trim()` for its side effect, then returns `out`.
- **`import` at top level only**, one per module that uses it.

See [`lib/examples/Energize.rhai`](https://github.com/inou/nrgize-rs/blob/main/lib/examples/Energize.rhai) for a complete,
runnable configuration (registry login, accessories, then `deploy()`).
