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

### 1.1 Multi-arch / cross-platform builds — **L** — steps 1–2 ✅ shipped, step 3 open

Steps 1–2 (single-platform builds + a preflight arch-mismatch check) are done:

1. ✅ `cfg.platform` (e.g. `"linux/amd64"`) on `docker_build`/`deploy()` — when
   set, uses `buildx build --platform <value> --load` instead of a plain
   `build`. See [Multi-arch builds](deploy.md#multi-arch-builds).
2. ✅ `deploy()` compares this machine's `uname -m` to `hosts[0]`'s (normalized,
   so macOS `arm64` and Linux `aarch64` aren't a false mismatch) and **throws**
   before build/push/pull if they differ and `cfg.platform` wasn't already
   set — LIVE runs only (the check is stubbed and skipped under `--dry-run`,
   same class of limitation as the rest of the live deploy path; see
   [Robustness Review](robustness-review.md) R8).

**Still open:**
3. Remote builder support: build on a designated host over SSH when local
   `buildx` can't target the platform (M). A genuine multi-platform MANIFEST
   LIST (comma-separated platforms, `--push` at build time instead of a
   separate push step) is also not supported by the current `cfg.platform` —
   it's a single target architecture only.

**Fast-follows noted in review (non-blocking):** the arch preflight only
probes `hosts[0]` — a mixed-architecture fleet passes the check and can still
hit an exec-format error on a *different* host at container start; a cheap
per-host probe loop (or fan-out) would close this. There's also no local
`buildx`-availability preflight (`docker buildx version`) — an old Docker
without the plugin fails mid-build with a raw shell error rather than a clear
early message; today only the docs warn about this.

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

### 1.5 Server bootstrap: `nrg setup` — **L** — steps 2 (`nrg remove`) and 3 (`doctor --host`) ✅ shipped, step 1 (`nrg setup` itself) open

**Current state:** nothing prepares a host. The user must hand-install Docker,
and the proxy/accessories only come up as a side effect of the first
`deploy()`. There is also no teardown.

**Why it matters:** Kamal's headline demo is "fresh Ubuntu box → running app in
one command". First-run experience is where adoption is won.

**Next steps:**
1. `nrg setup` (or a stdlib `bootstrap(hosts)` recipe): install Docker if
   absent, create the network, boot the proxy, start accessories (M).
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
3. ✅ Extend `nrg doctor` with `--host`: probes SSH reachability and
   container-runtime presence on each host before the first deploy (see 2.5).
   Registry-auth checking is still open.

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

**Known limitation:** the real lock host is `hosts[0]` of whatever array the
ACTUAL holding deploy/rollback call was given — an in-flight choice never
persisted anywhere. `nrg lock` only auto-detects a host when exactly one is
recorded in state for the service (Opus review, round 6 — auto-picking from
a multi-host, alphabetically-sorted list risked silently targeting the
wrong host); `--host` is required whenever more than one is recorded.

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

### 2.4 Runtime decryption of `ENC[...]` + secret-manager adapters — **M** — step 1 ✅ shipped

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
2. **Still open:** fetch adapters, Kamal-style — resolve `secret("X")`
   through a configurable command (1Password `op read`, Bitwarden, Vault,
   Doppler) so plaintext never lands on disk in the first place (M).

### 2.5 Preflight depth for `nrg doctor` — **S** — ✅ shipped (SSH + runtime; registry auth still open)

`nrg doctor [--host <host>]...` now preflights each host (SSH reachability,
then container runtime presence), in parallel, defaulting to every host
recorded in state when `--host` is omitted. See
[CLI reference](cli.md#nrg-doctor).

**Still open:** registry credential checking (e.g. can this host actually
`docker login`/pull the configured registry) isn't implemented — the
original scope's third check. Pairs with 1.5 (`nrg setup`) once that lands.

### 2.6 Deploy notifications / lifecycle hooks — **S**

**Current state:** `pre_deploy_cmd` exists inside `deploy()`'s cfg, but there
are no tool-level hooks and no notification story; teams that want a Slack
"v42 live on 3 hosts" message must hand-write `http_post` calls per project.

**Next steps:** optional `pre_deploy` / `post_deploy` / `post_rollback` hook
functions (called if defined in the orchestration file), plus a stdlib
`notify.rhai` with a generic webhook helper.

### 2.7 Accessory lifecycle — **S**

**Current state:** `accessory_run` (in `lib/deploy.rhai`) starts an accessory
if absent. There is no stop / restart / upgrade / logs path — the first
`postgres:16` → `postgres:17` bump has no supported route.

**Next steps:** `accessory_stop` / `accessory_restart` /
`accessory_upgrade(host, name, image, cfg)` (stop, keep volume, start new
image) in the stdlib, surfaced through the examples.

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

---

## Tier 3 — multipliers

### 3.1 Distribution: prebuilt binaries — **S**

**Current state:** install is "cargo build from source". The audience is app
developers, not Rust developers.

**Next steps:** GitHub Releases with prebuilt binaries (macOS arm64/x86_64,
Linux x86_64/arm64) built by CI on tag, an install script, a Homebrew tap, and
`cargo install nrg` as the fallback. Update README install section.

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

### 3.4 Templates: `nrg init --template <framework>` — **S**

**Current state:** `nrg init` scaffolds one starter file; framework examples
live in `lib/examples/` and must be copied by hand together with `lib/`.

**Next steps:** `nrg init --template rails|django|nextjs|phoenix|laravel`
writes the corresponding example as `Energize.rhai` (trivial once 3.2 removes
the vendoring step).

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

---

## Suggested sequencing

1. **Now:** 1.4 `status` ✅ → 2.3 audit log ✅ → 1.2 `logs` ✅ → 1.3 `app exec` ✅
   (small, state-driven, immediately visible; all four shipped), with
   2.4-step-1 (R3 fix) ✅ and 3.5 (R7 signal handling) ✅ folded in from the
   robustness review — both now shipped.
2. **Next:** 1.1 multi-arch builds (steps 1–2 ✅, step 3 open) → 1.5 `setup`
   (`nrg remove` + doctor `--host` ✅, `nrg setup` itself open) → 3.3
   `nrg rollback` ✅ → 2.1 distributed lock ✅ (including its `nrg lock` CLI).
3. **Then:** 2.2 destinations ✅ → 3.2 embedded stdlib ✅ → 2.8 maintenance
   mode ✅ → 3.1 binaries → 3.4 templates, with 2.6/2.7 slotted in as small
   wins.

The cut line for a credible `v0.2` announcement is the end of step 2: at that
point a new user on a Mac can bootstrap a fresh VPS and operate the app
day-to-day without leaving `nrg`.
