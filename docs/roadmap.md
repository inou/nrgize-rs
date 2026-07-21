---
title: Product Roadmap
nav_order: 98
---

# Product Roadmap — feature gaps and next steps

**Date:** 2026-07-10
**Scope:** Product features — what users of a Kamal-style deploy tool expect
and cannot do with `nrg` today.

This is the feature-side companion to the
[Robustness Review](robustness-review.md), which catalogs
*reliability* gaps in what already exists. This document catalogs *capability*
gaps: things the tool doesn't do at all yet.

## How to read this

The deploy engine itself is in good shape — dry-run simulation, secret
redaction, transactions with LIFO compensation, proxy pluggability, and the
runtime abstraction are ahead of comparable tools. The gap is that a user's
relationship with their app is 1% deploying and 99% operating, and `nrg`
currently serves only the 1%.

Items are grouped into three tiers:

- **Tier 1 — users churn without these.** Hit within the first hours of real
  use; each one is a reason to go back to Kamal.
- **Tier 2 — teams adopt because of these.** Not blocking for a solo first
  deploy, but blocking for a team running production.
- **Tier 3 — multipliers.** Distribution and DX work that amplifies everything
  else.

Each item names the current state (with file references) and the proposed next
step. Effort tags are rough: **S** (days), **M** (a week-ish), **L** (multiple
weeks).

---

## Tier 1 — users churn without these

### 1.1 Multi-arch / cross-platform builds — **L** — ✅ shipped (all steps, including 3a and 3b)

Steps 1–2 (single-platform builds + a preflight arch-mismatch check) are done:

1. ✅ `cfg.platform` (e.g. `"linux/amd64"`) on `docker_build`/`deploy()` — when
   set, uses `buildx build --platform <value> --load` instead of a plain
   `build`. See [Multi-arch builds](deploy.md#multi-arch-builds).
2. ✅ `deploy()` compares the BUILD machine's `uname -m` (this machine, or
   `cfg.build_host` if set) to `hosts[0]`'s (normalized, so macOS `arm64` and
   Linux `aarch64` aren't a false mismatch) and **throws** before
   build/push/pull if they differ and `cfg.platform` wasn't already set —
   LIVE runs only (the check is stubbed and skipped under `--dry-run`, same
   class of limitation as the rest of the live deploy path; see
   [Robustness Review](robustness-review.md) R8).
3. Step 3 split into two independently-scoped parts, both now shipped:
   - **3b ✅** A genuine multi-platform MANIFEST LIST: a comma-separated
     `cfg.platform` (e.g. `"linux/amd64,linux/arm64"`) makes `docker_build` use
     `buildx build --platform <list> --push` instead of `--load` (buildx can't
     `--load` more than one platform), publishing the manifest list straight to
     the registry during the build; `deploy()` detects the same comma and
     automatically skips its own separate `docker_push` step. See
     [Multi-arch builds](deploy.md#multi-arch-builds).
   - **3a ✅** Remote builder support: `cfg.build_host` runs the SAME build
     command (plain `build` or `buildx build`, single- or multi-platform) on a
     designated host over SSH instead of locally — e.g. a native arm64
     builder, so an arm64 target needs no buildx/qemu emulation at all. Shipped
     as a SEPARATE slice from 3b, after the codebase survey above found it was
     genuine greenfield (no build-host `cfg` concept, no context-sync
     primitive, `docker_build` hardwired to `local_exec`). The context-sync gap
     is filled from EXISTING primitives rather than new Rust-level transport:
     `local_exec`'s tar + `base64` piped to `ssh_exec_stdin`'s decode+extract —
     base64 isn't cosmetic, since a naive raw-bytes pipe through this
     codebase's `String`-based command I/O would silently corrupt binary tar
     data (see `sync_build_context`'s doc comment in `lib/docker.rhai`). The
     arch preflight (step 2) and the push step were both updated to target
     `build_host` instead of this machine when it's set — an easy miss that
     would have reintroduced exactly the false-mismatch/wrong-machine-push bug
     class this whole feature exists to prevent. See
     [Multi-arch builds](deploy.md#multi-arch-builds).

**Fast-follows noted in review (non-blocking):** the arch preflight only
probes `hosts[0]` — a mixed-architecture fleet passes the check and can still
hit an exec-format error on a *different* host at container start; a cheap
per-host probe loop (or fan-out) would close this. There's also no local
`buildx`-availability preflight (`docker buildx version`) — an old Docker
without the plugin fails mid-build with a raw shell error rather than a clear
early message; today only the docs warn about this. `cfg.build_host`'s context
sync buffers the whole (compressed) build context in memory on both ends —
fine for a typical app, but a large context (an unexcluded `node_modules`,
say) will be slow and memory-heavy; the `.dockerignore`-honoring exclude
mitigates the common case but isn't a hard limit.

### 1.2 Day-2 CLI: `nrg logs` — **M** — ✅ shipped

`nrg logs <service> [--host h] [--follow] [--lines n]` fans out one
`docker logs` per host over SSH in parallel, host-prefixed, non-interactive.
See [CLI reference](cli.md#nrg-logs).

### 1.3 Day-2 CLI: `nrg app exec` / interactive console — **M** — ✅ shipped

`nrg app exec <service> [--host h] [-i] [cmd...]` resolves the live container
from state (`<service>.target.<host>` keys, via the same `StateStore::hosts_for`
now shared with `nrg status`/`nrg logs`) and runs `docker exec` inside it —
non-interactively by default (exit code propagates, safe for scripts/CI), or
with `-i` via `ssh -t ... docker exec -it ...` (same process-replacement
pattern as `nrg ssh`) for a real console. See
[CLI reference](cli.md#nrg-app-exec).

**Fast-follows noted for a later pass:** neither command has a `--json`
output mode. `nrg app exec`'s host selection errors out on ambiguity (>1
host, no `--host`) rather than offering an interactive picker — reasonable
for scripting, less friendly for a human at a terminal. `nrg logs <service>`
took the service name as required rather than the originally-sketched
optional/fleet-wide form — a deliberate simplification, revisit if a
"tail everything" use case shows up. Nit: `nrg ssh`'s own "Connecting to…"
banner is still on stdout (unlike `nrg app exec`'s, moved to stderr during
review) — harmless since that command is interactive-only, but worth
tidying for consistency.

### 1.4 Day-2 CLI: `nrg status` — **S** — ✅ shipped

`nrg status [service] [--offline]` reads `.energize/state.json` and, unless
`--offline`, probes each host's canonical container over SSH (one
`docker inspect` per host) for running/health state. See
[CLI reference](cli.md#nrg-status).

**Fast-follows noted in review (non-blocking):** the container name probed is
hardcoded to `<service>-web`, so a custom container name reads as "not
deployed here" — worth a config hook. `nrg status` always exits `0` even when
a host is unreachable/unhealthy; a `--exit-code` (or `--check`) mode would let
CI treat an unhealthy fleet as a failure. No `--json` output yet for scripting.

### 1.5 Server bootstrap: `nrg setup` — **L** — ✅ shipped (all three steps)

**Was:** nothing prepared a host. The user had to hand-install Docker, and the
proxy/accessories only came up as a side effect of the first `deploy()`. There
was also no teardown.

**Why it matters:** Kamal's headline demo is "fresh Ubuntu box → running app in
one command". First-run experience is where adoption is won.

**Steps:**
1. ✅ `nrg setup --host h [--host h]... [--proxy kamal|caddy] [--proxy-version v]
   [--network name] [--yes] [--dry-run]`: install Docker if absent (the
   official `https://get.docker.com` convenience script over SSH, gated
   behind `--yes` since it's a consequential root-level action), create the
   network if `--network` is given (idempotent — new `docker_network_create`/
   `docker_network_create_all` in `lib/docker.rhai`), and boot the proxy —
   reusing the SAME stdlib `proxy_boot_all`/`caddy::proxy_boot_all` logic
   `deploy()` itself uses (via a synthesized script, the same architecture
   `nrg rollback` already established — see `eval::run_setup`), rather than
   reimplementing "start kamal-proxy" in Rust. `--host` is required (a fresh
   host has no recorded state yet to auto-discover a target from — the whole
   scenario this command exists for). Preflight (reachability + runtime
   presence) is native Rust over raw SSH, matching `nrg doctor --host`'s own
   probe; the network-create/proxy-boot half reuses `execute_with`/
   `wire_run`, so `--dry-run` shows the real `PlannedAction` plan and a live
   run gets the state lock and an audit-trail entry like every other
   side-effecting command.

   **Deliberately scoped narrower than this line's original wording, twice:**
   (a) does **not** start accessories — there is no manifest anywhere in this
   codebase recording which accessories a given service needs (`accessory_run`
   calls are entirely project-script-defined; see `docs/deploy.md`'s
   accessory lifecycle section), so there's nothing generic for a native
   command to auto-invoke; a project that wants accessories bootstrapped
   alongside `nrg setup` defines its own function and calls it separately via
   `nrg run <fn>`. (b) only auto-installs Docker, never Podman/nerdctl —
   Docker's official convenience script is the one well-known, stable,
   universally documented command for exactly this "fresh box" case;
   installing a different runtime is left to the operator. Both are the same
   "deliberately scoped narrower" precedent `nrg remove` (step 2, below) set.
2. ✅ `nrg remove <service> [--host h] [--yes] [--purge-state]`: force-remove
   a service's own container from each host it's deployed to,
   discovered the same way `nrg status`/`nrg logs`/`nrg app exec` already do
   (`StateStore::hosts_for`). See [CLI reference](cli.md#nrg-remove).
   Deliberately scoped narrower than this line originally proposed: it does
   **not** touch the host's shared proxy or accessories. The proxy
   (`kamal-proxy`/`caddy`) is one instance serving every service on a host —
   tearing it down as a side effect of removing ONE service would take down
   every OTHER service on that host, and nothing in state records which
   accessories a given service's deploy touched, so there's no way to
   identify "this service's accessories" safely without guessing. Proxy-route
   removal (`proxy_remove(host, service)` already exists in the stdlib) and
   accessory lifecycle are 2.7's job, once that finding gives accessories a
   real service-scoped identity to remove by.

   Both Opus and Fable's review rounds independently caught the same two real
   bugs before this shipped: (1) the "already absent = success" idempotency
   check only recognized Docker's capitalized `No such container` wording,
   silently reporting a real failure (and skipping `--purge-state`) on Podman,
   which emits it lowercase — fixed to lowercase-and-match `"no such"`,
   mirroring `sim.rs`'s existing Docker/Podman-aware classifier (robustness
   review R4/R31). (2) `--host <one-of-many> --purge-state` on a multi-host
   service deleted the service-wide `version`/`image`/`prev`/`deployed_at`
   keys globally even though only one host's container was touched — leaving
   `nrg status` reporting "no deploy recorded" for a service another,
   untouched host was still running and serving traffic on, with no rollback
   target left for it. Fixed: those shared keys are now only purged if this
   run covered every host the service is recorded as deployed to; a partial
   `--host` run keeps them and only clears the per-host entries it actually
   removed.
3. ✅ Extend `nrg doctor` with `--host`: probes SSH reachability,
   container-runtime presence, and registry auth on each host before the
   first deploy (see 2.5, now fully shipped).

---

## Tier 2 — teams adopt because of these

### 2.1 Distributed deploy lock — **M** — ✅ shipped (robustness review R15)

**Current state (2026-07-10):** `deploy()` (and `rollback()`, which calls it
internally) now takes a server-side lock on the FIRST app host — an atomic
`mkdir /tmp/nrg-deploy-lock-<service>` acquired before any build/push/pull/roll
work and released once the whole deploy finishes, success or failure — closing
the cross-machine race the local flock (`docs/safety.md`) never could. On by
default; `cfg.skip_lock: true` opts out. See
`docs/safety.md#cross-machine-deploy-lock-robustness-review-r15` for the full
design and its known limitations (no automatic staleness/TTL — a crashed or
SIGINT'd control process leaves the lock held for manual cleanup, same
tradeoff as the local flock's own `NRG_STATE_LOCK` staleness gap).

**Now:** `nrg lock status|acquire|release <service> [--host h]` (the Kamal
model) — manual control without SSHing to the lock host by hand. `status` is
a read-only probe; `acquire` lets an operator take the lock deliberately (e.g.
to block automated deploys during a maintenance window) without running a
deploy; `release` (gated behind `--yes`) force-clears a stale lock left by a
crashed or SIGINT'd run. Implemented as a native Rust command (no Rhai engine
involved) that mirrors `acquire_deploy_lock`/`release_deploy_lock`'s exact
directory/holder-file convention, so a lock taken by a real `deploy()` call
and one taken by `nrg lock acquire` are indistinguishable to each other. See
[CLI reference](cli.md#nrg-lock).

**Fixed (full-project Fable review):** the lock host used to be `hosts[0]` of
whatever array the calling deploy/rollback was given — an in-flight choice,
never persisted, and NOT order-independent: two operators deploying the same
fleet with a differently-ordered host array (e.g. `["web2","web1"]` vs
`["web1","web2"]`) took the lock on different hosts, silently defeating the
mutual exclusion this lock exists to provide. `deploy()`/`rollback()` now
anchor the lock on the **alphabetically-first** host instead (a sorted copy
of `hosts`, never mutating the caller's own array/deploy order) — the same
convention `StateStore::hosts_for` already used for `nrg lock`'s own
auto-detect (see below), so the two now agree by construction.

`nrg lock` still only auto-detects a host when exactly one is recorded in
state for the service (Opus review, round 6 — at the time, auto-picking from
a multi-host, alphabetically-sorted list risked mismatching whatever host
the actual lock happened to be on); `--host` is required whenever more than
one is recorded. Now that the lock host is deterministically the
alphabetically-first recorded host, that restriction could be relaxed to
auto-detect via the same sort — left as a small follow-up, not done here to
keep this fix scoped to the mismatch it was reported for.

### 2.2 Environments / destinations — **M** — ✅ shipped

**Was:** no first-class staging vs. production. Two environments deployed
from the same directory shared one `state.json` keyspace, so
`<service>.version` from staging could be read (and clobbered) by a
production rollback.

**Now:**
1. ✅ State is namespaced by destination: `StateStore` gained a `dest` field
   and transparently prefixes every key `<dest>/<key>` on disk (e.g.
   `staging/app.version`) while `get`/`set`/`del`/`services()`/`hosts_for()`/
   `all()` still address keys by their plain name — the namespace prefix is
   invisible to callers on both the Rust and Rhai side. One destination's
   `services()`/`all()` never sees another's keys, even though both live in
   the SAME shared `state.json` (one file, one lock, one backup). `None`/
   `"default"` is byte-for-byte identical to no destination at all — full
   backward compatibility for every existing single-destination project.
2. ✅ `--dest <name>` on `nrg exec`/`nrg run`/`nrg rollback`, exposed to
   scripts as `nrg_dest()` (returns `"default"` when unset). A destination
   name must be non-empty and contain only letters/digits/`-`/`_` — checked
   once at the CLI boundary, since a destination also names a
   `.energize/secrets.<dest>` FILENAME SUFFIX (below), so this rules out
   `/`/`..` path traversal by construction, not just convention.
3. ✅ Per-destination secrets file convention: `secret(name)` checks (in
   order) `$NRG_SECRET_<NAME>`, then — only when `--dest` is set —
   `.energize/secrets.<dest>`, then the shared `.energize/secrets`, then
   `.env`. A destination's file only needs the keys that actually differ
   per environment; anything it doesn't mention still resolves from the
   shared file.
4. ✅ Documented in [CLI reference](cli.md#environments--destinations) with a
   worked example (state namespacing, `nrg_dest()`, the secrets file
   convention, and which commands don't support `--dest` yet).

**Known limitation:** `nrg status`/`nrg logs`/`nrg app exec`/`nrg remove`/
`nrg lock`/`nrg doctor` don't have a `--dest` flag yet — they only ever see
the default (unnamespaced) destination's state. A project that deploys
exclusively via `--dest` won't have those hosts discovered by those commands
until they gain the same flag.

**Known limitation (Fable review, round 7):** `--dest` isolates
`state.json` only — it does not namespace the container itself.
`lib/deploy.rhai`'s canonical container name (`<service>-web`) and the R15
deploy lock (`/tmp/nrg-deploy-lock-<service>`) are both destination-
independent, so two destinations of the same service deployed to the SAME
host still clobber each other's live container even though each
destination's own state correctly records its own deploy as healthy. This
feature is designed for (and requires) giving each destination a disjoint
fleet of hosts — documented in
[CLI reference](cli.md#what-doesnt-yet-support---dest).

### 2.3 Deploy history / audit trail — **S** — ✅ shipped (invocation-level)

Every LIVE `nrg exec`/`nrg run` now appends a JSON line to
`.energize/audit.log` (timestamp, user@host, command, target/args, outcome —
secret-redacted), printed by `nrg audit [filter] [--limit N]`. See
[CLI reference](cli.md#nrg-audit).

**Still open:** this records *invocations*, not deploy semantics — it doesn't
yet know "service X went from image A to image B" the way a
`deploy()`-aware audit would, so "rollback to any prior version" (beyond the
single `<service>.prev` snapshot) is still future work.

**Fast-follows noted in review (non-blocking):** `[filter]` only matches
target/args/file, not user/host/outcome. No `--json` output yet for
scripting. Redaction only catches a CLI arg that matches a value the script
*also* resolved via `secret()` during that same run — a secret typed as a
raw CLI arg in a script that never calls `secret()` for it won't be caught
(inherent to how redaction is keyed; documented in `src/audit.rs`).

### 2.4 Runtime decryption of `ENC[...]` + secret-manager adapters — **M** — ✅ shipped (both steps)

1. ✅ **Fixed R3** — `secret()` now transparently decrypts an `ENC[...]` value
   via the discovered `.nrg-key` before it's ever used, throwing a clear error
   if no key is found or decryption fails. Fixing this also surfaced a second
   bug in the same workflow: `age -a`'s armored output is multi-line PEM,
   which can't survive being pasted into a single `KEY=VALUE` line — the
   token is now `|`-joined into one line by `encrypt_value` (and reversed by
   `decrypt_value`), so the documented "paste `ENC[...]` into `.env`"
   workflow actually works end-to-end now, covered by a real `age`-gated
   round-trip test. Still plaintext-on-disk between decrypt and use (in
   memory / off-argv, per the existing `Secret` contract) — this only closes
   the "never decrypted" gap, not the broader "secrets live on disk at all"
   design.

   **Fast-follows noted in review (non-blocking):** a token pasted with its
   closing `]` lost (e.g. `ENC[...` truncated mid-paste) silently passes
   through as a "plain" value instead of erroring — a narrower recurrence of
   R3's original failure shape for a corrupted paste; warning on an
   `ENC[`-prefixed value missing its closing bracket would be safer.
   `decrypt_value` uses lossy UTF-8 decoding on the decrypted plaintext,
   which is correct for `nrg`'s own string-only encrypt path but would
   mangle a binary secret encrypted directly with raw `age`.
2. ✅ **Fetch adapters, Kamal-style** — a `CMD[command]`-framed value (checked
   wherever a raw value can come from: env var, per-dest file, shared file, or
   `.env`) runs `command` locally via the same `CommandRunner` every other
   builtin uses, and its trimmed stdout becomes the secret — so `op read`,
   `vault kv get`, `bw get password`, `doppler secrets get`, or anything else
   with a CLI all work without `nrg` needing a per-backend config schema.
   Deliberately reuses the existing `ENC[...]` bracket-framing convention
   (rather than Kamal's own shell `$(...)` syntax) so every "special" value in
   a secrets file stays recognizable the same way, and avoids ambiguity with a
   legitimate value that happens to contain a literal `$(...)` substring.
   Throws (including the command's stderr) on a nonzero exit, and flows
   through the exact same length-check/redaction pipeline a file/env-sourced
   value already does. See [Builtins reference](builtins.md#secretname---secret).

### 2.5 Preflight depth for `nrg doctor` — **S** — ✅ shipped (all three checks)

`nrg doctor [--host <host>]...` now preflights each host (SSH reachability,
then container runtime presence, then registry auth), in parallel, defaulting
to every host recorded in state when `--host` is omitted. See
[CLI reference](cli.md#nrg-doctor).

The third check (registry credential checking) re-checks images ALREADY
recorded in `.energize/state.json` (each service's `<svc>.image`, set by
`deploy()`) via `docker manifest inspect <image>` over SSH — a lightweight
registry-API round trip, not a full pull, that fails exactly the way a real
`deploy()`/`accessory_run` pull would if credentials are missing or wrong on
that host. Deliberately narrower than the literal "can this host `docker
login`" wording, twice: there's no separate "which registry does this
project use" concept anywhere in this codebase to check independently of a
deployed image, so a fresh host with nothing deployed to it yet has nothing
to check here; and it only runs when the host's detected runtime looks like
Docker (`docker manifest inspect` is Docker-specific syntax — skipped, not
run-and-misreported, on a Podman/nerdctl host, the same scope narrowing `nrg
setup`'s Fable review established for its own Docker-only network/proxy-boot
step).

### 2.6 Deploy notifications / lifecycle hooks — **S** — ✅ shipped

**Was:** `pre_deploy_cmd` existed inside `deploy()`'s cfg, but there were no
tool-level hooks and no notification story; teams that wanted a Slack "v42
live on 3 hosts" message had to hand-write `http_post` calls per project.

**Now:** three OPTIONAL Rhai functions the orchestration file may define —
`hook_pre_deploy(service, image, hosts)` (may throw to block the deploy,
before any work happens), `hook_post_deploy(service, image, hosts)`, and
`hook_post_rollback(service, image, hosts)` (both best-effort — a throw is
warned, not fatal, matching `post_deploy_cmd`'s own convention). Looked up
by exact name **and** arity via Rhai's `is_def_fn`/`Fn(name).call(...)`, so
this needed no new engine-level plumbing — verified empirically that this
correctly reaches from a stdlib module function back into the top-level
orchestration file's own functions before implementing. Named `hook_*`
(not `pre_deploy`/`post_deploy`) to avoid colliding in the reader's head with
the existing, unrelated `cfg.pre_deploy` (an in-container release command)
and `cfg.pre_deploy_cmd`/`cfg.post_deploy_cmd` (raw host shell) keys.

Plus a new `lib/notify.rhai` stdlib module (`notify::webhook(url, payload)`,
`notify::slack(url, text)`) — a thin, dry-run-safe wrapper over `http_post`
so a hook doesn't need to hand-write JSON escaping.

**Caveat:** `nrg rollback` (roadmap 3.3) synthesizes its own standalone
script and never evaluates the orchestration file, so `hook_post_rollback`
only fires when `rollback()` is called from within the orchestration file's
own code (a project-authored task, or a direct `deploy::rollback(...)`
call) — not via the native CLI command. See
[`docs/deploy.md`](deploy.md#lifecycle-hooks) for the full contract.

### 2.7 Accessory lifecycle — **S** — ✅ shipped

**Was:** `accessory_run` (in `lib/deploy.rhai`) starts an accessory if absent,
but there was no stop / restart / upgrade path — the first `postgres:16` →
`postgres:17` bump had no supported route, since `accessory_run`'s own
"already running" check is by name only and can never itself notice an image
bump.

**Now:** `accessory_stop(host, name)` / `accessory_restart(host, name)` /
`accessory_upgrade(host, name, image, cfg)` (+ 3-arg overload) in
`lib/deploy.rhai`:
- `accessory_stop` stops the container without removing it (named volumes and
  bind mounts untouched), idempotent on an already-stopped accessory.
- `accessory_restart` restarts the existing container in place (`docker
  restart`), reusing its already-configured image — no `image` argument by
  design, since Docker's own `restart` can't change what image a container
  runs.
- `accessory_upgrade` pulls the new image first (so a bad tag or
  registry-auth failure surfaces before the old container is touched), then
  stops and removes the old container (never `-v`, so named volumes
  survive), then starts the new image fresh via `accessory_run` itself,
  reusing its start-and-verify logic.

All three go through sim-routed `docker::` wrappers (including a new
`docker_restart`), so a `--dry-run` plan for any of them reflects the same
outcome a live run would produce, rather than diverging on a stale
pre-mutation probe.

See [`docs/deploy.md`](deploy.md#accessory_stophost-name--accessory_restarthost-name--accessory_upgradehost-name-image-cfg)
for the full contract. Not added to `lib/examples/*.rhai` — those files'
top level unconditionally deploys on every evaluation (see 2.8's note on the
same hazard), and lifecycle calls like these are exactly the kind of
standalone, `nrg run`-able task that pattern warns against mixing in.

### 2.8 Maintenance mode — **S** — ✅ shipped

**Was:** kamal-proxy supports maintenance pages natively; neither
`lib/proxy.rhai` nor `lib/caddy.rhai` exposed it.

**Now:** `proxy_maintenance(host, service, on_off, cfg)` in both proxy
backends (same-surface contract):
- kamal-proxy: `kamal-proxy stop <service> --drain-timeout=<cfg.drain_timeout>`
  (default `"30s"`) suspends the route without forgetting its target;
  `kamal-proxy resume <service>` restores it — no extra info needed.
- Caddy (no native suspend/resume): maintenance-on/-off PATCH only the
  route's `handle` sub-path (leaving `match`/domain untouched — PATCHing the
  whole route, an earlier version of this, would silently drop the TLS host
  match); maintenance-off requires `cfg.target` since Caddy can't remember
  what the handle used to point at once it's replaced.

See [`docs/deploy.md`](deploy.md#maintenance-mode-proxy_maintenancehost-service-on_off-cfg)
and [`docs/stdlib.md`](stdlib.md#maintenance-mode) for the full contract and a
`nrg run`-able task pattern (deliberately NOT added to `lib/examples/*.rhai`,
whose top level unconditionally deploys — see the note there about why that
would make `nrg run maintenance` trigger a full redeploy as a side effect).

**Fixed (full-project Fable review):** `proxy_boot` on both backends used to
always float the proxy container's OWN image — `basecamp/kamal-proxy:latest`
or `caddy:2` — with no way for a `deploy()` caller to pin it, even though
this container holds every service's production traffic on that host. A
broken upstream push could auto-install on any host that (re)booted the
proxy after it landed, and hosts booted at different times could silently
end up running different proxy versions. `cfg.proxy_version` now selects the
pulled tag on both backends (kamal-proxy: `"latest"` default, e.g.
`"v0.9.2"`; Caddy: `"2"` default, e.g. `"2.8.4"`) and threads through
`deploy()`'s own `cfg` unchanged. Leaving kamal-proxy on the default
`"latest"` still prints a soft warning (the same R10-style convention used
for app images) — Caddy's own default is already a major-version pin, so it
doesn't warn. `cfg.proxy_version` is `sh_quote()`d before it reaches the
`docker pull`/`docker run` commands on both backends (Fable review pass 2)
— it's caller-supplied data flowing into a remote shell, same as every
other cfg-derived string in this codebase (issue #10's convention).

---

### 2.9 PaaS provider targets (Bunny Magic Containers, similar platforms) — **L** — ✅ shipped (all four phases)

**Was / is:** `nrg` deploys exclusively to SSH-reachable hosts running Docker — the entire engine
(`CommandRunner::run_ssh`, `lib/deploy.rhai`, `lib/proxy.rhai`) is built on that one primitive. A
managed container PaaS like Bunny Magic Containers has no SSH surface at all; it's driven entirely
through a REST API. This surfaced from a real use case: a multi-tenant SaaS control plane wanting
fleet-wide image upgrades across potentially hundreds of per-tenant containers on Bunny, not a
single-app deploy.

See [the design spec](superpowers/specs/2026-07-12-bunny-magic-containers-design.md) for the full
decision record and phase breakdown. Four phases, in order:

1. **HTTP builtin: headers + `http_put`/`http_patch`/`http_delete`** — ✅ shipped. Every verb
   (`http_get`/`http_post`/`http_put`/`http_patch`/`http_delete`) now takes an optional trailing
   `headers` map (`#{"Authorization": "Bearer " + token}`), enough to drive a real authenticated
   REST API from Rhai stdlib alone. See [Phase 1 plan](superpowers/plans/2026-07-12-bunny-phase1-http-client.md)
   and [the HTTP builtins reference](builtins.md#http). `to_json`/`from_json` (roadmap-adjacent,
   already shipped) mean the JSON side of this was already solved; this closed the transport gap —
   no new Rust builtin is needed for Phase 2's provider module itself.
2. **`lib/bunny.rhai` stdlib module** — ✅ shipped. Single-target image upgrade (`deploy_app`),
   status poll (`current_image_tag`/`wait_for_image`), and rollback-to-previous-tag
   (`rollback_app`), built entirely on Phase 1's primitives — zero new Rust, per the
   zero-vendoring stdlib philosophy already established for `deploy.rhai`/`proxy.rhai`. The
   request/response shapes are grounded in Bunny's own public GitHub Action source
   (`BunnyWay/actions/container-update-image`), not guessed — one field (`imageTag` on the GET
   response) is corroborated by a second independent public source (Phase 5), not
   live-account-tested. See
   [Phase 2 plan](superpowers/plans/2026-07-13-bunny-phase2-stdlib-module.md) and
   [the stdlib reference](stdlib.md#libbunny--bunny-magic-containers).
3. **Fleet-scale rollout** — ✅ shipped. `bunny::deploy_fleet(targets, cfg)`: a small canary slice
   deploys sequentially and is fully verified first, then the rest of the fleet PATCHes
   concurrently in configurable-size batches via a new generic `http_patch_all` builtin (mirroring
   `ssh_exec_all`'s own OS-thread fan-out — the one piece of this phase NOT constrained to
   "no new Rust", since genuine wall-clock concurrency across independent HTTPS requests needs
   real OS threads, which Rhai itself cannot express). A configurable `max_failures` threshold
   aborts the whole run — naming every failed target, without dispatching any further batch — the
   moment it's exceeded; a per-target failure below threshold is reported, never thrown. See
   [Phase 3 plan](superpowers/plans/2026-07-13-bunny-phase3-fleet-rollout.md),
   [the stdlib reference](stdlib.md#libbunny--bunny-magic-containers), and
   [the HTTP builtins reference](builtins.md#http).
4. **Volume-pinning guardrails** — ✅ shipped. Bunny volumes are pinned per-replica — an
   auto-scaled or relocated replica gets a fresh, empty volume. `deploy_app`, `rollback_app`, and
   `deploy_fleet` (per-target and on the shared fleet `cfg`) all refuse — by construction, not just
   a doc warning — any map containing `region`/`replicas`/`replica_count`/`scale`/`zone`, naming
   the offending key. This module never had a scale/region *operation* to guard (every function
   here only ever touches an app's image) — the guardrail closes the gap where a caller could
   reasonably (and wrongly) believe passing one of those keys through `nrg` does something, turning
   a silent no-op into a loud refusal. Also ships a worked dynamic-target-discovery example (`http_get`
   against an external tenant registry, building `targets` at runtime) — a documented pattern, not a
   new engine capability, per the design spec's D6. See
   [Phase 4 plan](superpowers/plans/2026-07-13-bunny-phase4-volume-guardrails.md) and
   [the stdlib reference](stdlib.md#libbunny--bunny-magic-containers).

**What's already reusable, verified against the real source:** multi-arch build/push already
targets `linux/amd64` (Bunny's requirement); `to_json`/`from_json` already exist; `http_get`
already does honest dry-run-aware health polling; the fleet-atomic roll/health-gate/rollback shape
in `lib/deploy.rhai` is conceptually correct for this and doesn't need reinventing, only a new
low-level primitive under it.

**Phase 5 (found during a post-ship feasibility review) — ✅ shipped.** Every phase above requires an
already-existing Bunny `app_id` — there was no `bunny::create_app`/`delete_app`, so `nrg` couldn't
own a tenant's full Bunny lifecycle, only its image upgrades. `create_app(cfg) -> map` provisions a
brand-new app (exactly one container, one pinned region, one replica — D9 extends the Phase 4
volume-pinning guardrail to provisioning time: `create_app` never exposes an autoscaling or
multi-region knob at all, unconditionally); `delete_app(cfg) -> HttpResponse` permanently deletes
one. Both built entirely on the already-shipped `http_post`/`http_delete` builtins — zero new Rust.
Also resolves Phase 2's flagged `imageTag` inference: it's now corroborated by a second independent
public source (Bunny's own official Terraform provider Go source,
`BunnyWay/terraform-provider-bunnynet`), not just the GitHub Action.

**Post-ship fix (live-account-verified):** `create_app`'s first live test (a real Bunny account,
region `PL`) failed 400 four separate ways the Terraform provider's Go source alone didn't surface:
`regionSettings` must omit `allowedRegionIds` entirely rather than send `[]` (Bunny enforces
`requiredRegionIds` ⊆ `allowedRegionIds`; an empty array makes that unsatisfiable and Bunny
misreports it as a missing-field error, not the actual subset-constraint one); `runtimeType` is
required and only accepts the literal `"Shared"` (matching `bunnynet`'s own hardcoded value);
`imagePullPolicy` is a required per-container field (`"IfNotPresent"`, not previously sent at all);
and `volumeMounts`' path key is `mountPath`, not `path`. All four fixed and covered by tests. See the
[Phase 5 design spec](superpowers/specs/2026-07-13-bunny-provisioning-design.md),
[Phase 5 plan](superpowers/plans/2026-07-13-bunny-phase5-provisioning.md), and
[the stdlib reference](stdlib.md#libbunny--bunny-magic-containers).

---

## Tier 3 — multipliers

### 3.1 Distribution: prebuilt binaries — **S** — ✅ shipped (release pipeline); tap population still open

**Was:** install was "cargo build from source" only. The audience is app
developers, not Rust developers.

**Now:**
1. ✅ `.github/workflows/release.yml`: pushing a `vX.Y.Z` tag builds prebuilt
   binaries for macOS arm64/x86_64 and Linux x86_64/arm64 (both macOS targets
   cross-compile from one `macos-14` runner rather than depending on GitHub's
   Intel macOS runners, which are on a deprecation path; linux-arm64 uses the
   free hosted `ubuntu-24.04-arm` runner), verifies the tag matches
   `Cargo.toml`'s version before building anything, packages each as a
   single-file `nrg-<target>.tar.gz` + `.sha256`, and publishes a GitHub
   Release with all four archives, their checksums, and a combined
   `checksums.txt`.
2. ✅ `scripts/install.sh`: `curl -fsSL .../install.sh | sh` detects OS/arch,
   downloads the matching release asset (`--version`/`$NRG_VERSION` to pin
   one), **verifies its sha256 checksum before installing anything**, and
   installs to `~/.local/bin` (`--bin-dir`/`$NRG_INSTALL_DIR` to change that).
   POSIX `sh` only (no bashisms). See [README](../README.md#installation).
3. ✅ `homebrew/nrg.rb`: a Homebrew Formula template checked into this repo —
   correct structure (`on_macos`/`on_linux` × `on_arm`/`on_intel`, matching
   the release workflow's four targets), but its `version`/`sha256` values
   are placeholders until a maintainer cuts a real tagged release and copies
   the updated values into an actual tap repository (conventionally
   `inou/homebrew-nrg`, so `brew tap inou/nrg` resolves it) — that tap repo
   itself has **not** been created as part of this slice (see below).
4. `cargo install nrg` fallback: **still open** — requires publishing the
   crate to crates.io (a `cargo publish` with a registry API token), which is
   a distinct, credentialed, one-way action outside this slice's scope.
5. ✅ README/`docs/getting-started.md` install sections updated to lead with
   the prebuilt-binary path, with source-build kept as the fallback.

**Still open / needs a maintainer:** cutting the first real `vX.Y.Z` tag (so
the release workflow actually produces real binaries/checksums to verify
against); creating the `inou/homebrew-nrg` tap repository and populating
`Formula/nrg.rb` with that release's real checksums; publishing to
crates.io. None of these are safe to do unattended — a git tag triggers a
real, user-facing GitHub Release, and a crates.io publish is irreversible.

### 3.2 Embedded stdlib — **M** — ✅ shipped

**Was:** `lib/` had to be manually vendored (`cp -r lib`) next to every
orchestration file, and never received updates afterwards — every project
drifted to its own stdlib fork.

**Now:**
1. ✅ Every core `lib/*.rhai` module (`docker`/`deploy`/`proxy`/`caddy`/
   `healthcheck`/`registry`/`runtime`/`recipe` — not `lib/examples/*`, which
   are full sample `Energize.rhai` files copied by hand, not library modules)
   is embedded in the binary via `include_str!` and resolved as
   `import "std/docker" as docker;` etc. — version-locked to the binary, zero
   `lib/` vendoring required. `import "lib/X"` (a real file on disk) keeps
   working exactly as before this feature existed — the two namespaces are
   disjoint prefixes, so neither ever silently falls back to the other.
2. ✅ `nrg rollback` (which synthesizes its own `import "…/deploy"`) now uses
   `"lib/deploy"` when a real, vendored `lib/deploy.rhai` exists at the
   resolved directory (a customized copy always wins), else falls back to
   the embedded `"std/deploy"` — so it now works with **zero vendoring
   required**, closing a real gap: it used to be the one native command that
   actually required a full vendored `lib/` to exist at all.
3. ✅ `nrg vendor [--force]` materializes the embedded stdlib onto disk as
   `lib/*.rhai`, for a project that wants to customize a module — not
   required for normal use. See [CLI reference](cli.md#nrg-vendor).

Fast-follow-worthy but out of scope for this slice: `nrg init`'s scaffolded
template still uses only builtins (no `import` at all), so it isn't affected
either way; a future `--template` (roadmap 3.4) would want its examples
switched from `import "lib/…"` to `import "std/…"` to drop their own
vendoring requirement too.

### 3.3 First-class `nrg rollback` — **S** — ✅ shipped

**Was:** only reachable as `nrg run rollback`, which required the project's
own `Energize.rhai` to define a `rollback` wrapper function, and gave it no
discoverability at the panic moment.

**Now:** `nrg rollback <service> [--host h]... [--image tag] [--dry-run]
[--lock-timeout secs]` calls the stdlib's `deploy::rollback(hosts, service,
cfg)` directly — no project-authored wiring needed. Hosts default to every
host `.energize/state.json` records for the service (same lookup `nrg
remove` uses); `--image` overrides the stdlib's own snapshotted `.prev`.
Reuses `nrg exec`/`nrg run`'s `execute_with` wiring (state lock, `--dry-run`
overlay, R7 interrupt handling, audit trail) rather than reimplementing any
of it, since `deploy()` (which `rollback()` calls internally) is a real,
side-effecting, interruptible operation. See
[CLI reference](cli.md#nrg-rollback).

**Still open:** rollback only ever targets the single snapshotted
`<service>.prev` or an explicit `--image` override — "rollback to any prior
version" (browsing deploy history) still depends on 2.3, which remains open
for the same reason noted there.

### 3.4 Templates: `nrg init --template <framework>` — **S** — ✅ shipped

**Was:** `nrg init` scaffolds one starter file; framework examples live in
`lib/examples/` and had to be copied by hand together with `lib/` (`cp -r
lib`), since they `import "lib/recipe"` — the on-disk convention.

**Now:** `nrg init --template rails|django|nextjs|phoenix|laravel` writes the
corresponding `lib/examples/*.rhai` starter as `Energize.rhai` directly — with
its `recipe` import switched to the embedded stdlib (`import "std/recipe" as
recipe;`, roadmap 3.2), so the result works with **zero vendoring**, unlike
hand-copying the same file. An unrecognized `--template` value is rejected
(by `clap`'s own enum validation) before anything is written; the existing
refuse-to-overwrite guard still applies. See
[CLI reference](cli.md#nrg-init).

### 3.5 Signal handling for the atomic promise — **M** — ✅ shipped

Robustness review **R7**, listed here because users *perceive* it as a product
promise: Ctrl-C mid-deploy used to run zero compensations, undermining
"fleet-atomic". SIGINT/SIGTERM now flip a flag the engine polls between
operations; a set flag ends the script as a normal `Err`, so an enclosing
`transaction()` unwinds exactly as it would for a `throw`. See
[Safety Features](safety.md#4-transactions) for the exact scope: this can't
preempt a single blocking `ssh_exec`/`local_exec`/`http_get` call mid-flight
(the still-open command-timeout gap), but responds within about one iteration
of a bounded retry loop (e.g. a health check wait) — the realistic
"stuck mid-deploy" case.

### 3.6 Apple `container` runtime priority on macOS — **S** — ✅ shipped

**Was:** `lib/runtime.rhai`'s `container_cmd()` resolves docker → podman → nerdctl, a single choice
used uniformly for BOTH local build/push commands (`docker_build`'s local branch, `docker_push`'s
local overload) and every remote SSH-invoked command (`docker_pull`/`docker_run`/`docker_stop`/
etc.). Apple shipped its own native container tool
([`container`](https://github.com/apple/container), macOS 26+, Apple Silicon) — but it can only
ever run on the LOCAL machine, never on a remote Linux deploy host, so it couldn't be added as a
plain new `set_runtime()` value without breaking every remote command the moment a macOS user with
the tool installed called `set_runtime("container")` (or an auto-detect that considered it).

**Now:** a genuinely separate, LOCAL-only resolution axis —
[`rt::local_build_cmd()`/`rt::set_local_build_runtime()`](stdlib.md#local-build-runtime-apples-container-tool-macos)
— used only by `docker_build`'s local branch and `docker_push`'s local-machine overload. On macOS,
when Apple's tool reports healthy (`container system status`), it's preferred there BEFORE Docker/
Podman/nerdctl, with zero effect on `container_cmd()` or any remote command: a `build_host`/`host`
is always a Linux box, which can never run Apple's tool, so those branches are untouched by
construction, not merely by convention. Handles the two real CLI-shape differences this surfaced
(grounded in Apple's own `apple/container` README/`docs/command-reference.md`, not guessed): native
`--platform` on plain `build` (no buildx-equivalent — a comma-separated MULTI-platform manifest-list
build is passed through as-is, expected but not confirmed to fail at the shell, same honest
treatment as the existing nerdctl+buildx caveat) and `image push` instead of a flat `push`.

---

## Suggested sequencing

1. **Now:** 1.4 `status` ✅ → 2.3 audit log ✅ → 1.2 `logs` ✅ → 1.3 `app exec` ✅
   (small, state-driven, immediately visible; all four shipped), with
   2.4-step-1 (R3 fix) ✅ and 3.5 (R7 signal handling) ✅ folded in from the
   robustness review — both now shipped.
2. **Next:** 1.1 multi-arch builds (all steps, including 3a/3b, ✅) → 1.5 `setup`
   (`nrg remove` + doctor `--host` + `nrg setup` itself — all ✅) → 3.3
   `nrg rollback` ✅ → 2.1 distributed lock ✅ (including its `nrg lock` CLI).
3. **Then:** 2.2 destinations ✅ → 3.2 embedded stdlib ✅ → 2.8 maintenance
   mode ✅ → 2.7 accessory lifecycle ✅ → 2.6 lifecycle hooks ✅ → 3.4
   templates ✅ → 3.1 binaries ✅ (release pipeline/install script/Homebrew
   formula template shipped; cutting the first real tag, creating the tap
   repo, and a crates.io publish remain manual, maintainer-only follow-ups).

The cut line for a credible `v0.2` announcement is the end of step 2: at that
point a new user on a Mac can bootstrap a fresh VPS and operate the app
day-to-day without leaving `nrg`.

4. **New axis, not blocking the above:** 2.9 PaaS provider targets (Bunny Magic Containers) —
   Phase 1 (HTTP builtin headers + verbs) is small, fully scoped, and independently shippable
   any time; Phases 2–4 (the actual provider module, fleet-scale rollout, volume guardrails) build
   on it in order. This doesn't compete with the SSH+Docker roadmap above — it's a second deploy
   target, not a replacement for the first.
