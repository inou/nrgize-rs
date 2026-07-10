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

### 1.1 Multi-arch / cross-platform builds — **L**

**Current state:** `docker_build` in `lib/docker.rhai` shells out to a plain
local `<runtime> build` — no `buildx`, no `--platform`, no remote builder, no
cache configuration.

**Why it matters:** the single most common setup for the target audience is
"build on an Apple Silicon laptop, deploy to an x86 VPS". Today that fails on
the host with an opaque exec-format error at container start — after the build,
push, and pull have all appeared to succeed. Kamal treats this as core
(`builder: arch:`, remote builders over SSH, registry cache).

**Next steps:**
1. Add `platform` to the `docker_build` cfg map; use `buildx build --platform`
   when present (S).
2. Detect the local-arch vs. host-arch mismatch in `deploy()` and fail at plan
   time with a clear message instead of at container start (S).
3. Remote builder support: build on a designated host over SSH when local
   `buildx` can't target the platform (M).

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
for scripting, less friendly for a human at a terminal.

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

### 1.5 Server bootstrap: `nrg setup` — **L**

**Current state:** nothing prepares a host. The user must hand-install Docker,
and the proxy/accessories only come up as a side effect of the first
`deploy()`. There is also no teardown.

**Why it matters:** Kamal's headline demo is "fresh Ubuntu box → running app in
one command". First-run experience is where adoption is won.

**Next steps:**
1. `nrg setup` (or a stdlib `bootstrap(hosts)` recipe): install Docker if
   absent, create the network, boot the proxy, start accessories (M).
2. `nrg remove`: stop and remove app containers, proxy, and accessories;
   optionally clear state (S).
3. Extend `nrg doctor` with `--hosts`: probe SSH reachability, Docker presence,
   and registry auth on each host before the first deploy (S — see 2.5).

---

## Tier 2 — teams adopt because of these

### 2.1 Distributed deploy lock — **M**

**Current state:** locking is a local `flock` on `<root>/.energize/state.lock`
(see `docs/safety.md`). It serializes runs on *one machine* only. Two teammates
— or a laptop plus CI — deploying concurrently from different machines
interleave freely, and the fleet-atomic transaction model makes that extra
dangerous: two transactions can unwind each other's containers.

**Next steps:** a server-side lock taken on the first app host (lock dir or
container label with holder + timestamp), checked before any mutating run, plus
`nrg lock acquire/release/status` for manual control — the Kamal model. The
local flock stays as the intra-machine layer.

### 2.2 Environments / destinations — **M**

**Current state:** no first-class staging vs. production. Users hand-roll it
with env vars and separate functions, and two environments deployed from the
same directory share one `state.json` keyspace, so `<service>.version` from
staging can be read by a production rollback.

**Next steps:**
1. Namespace state by destination (`staging/<service>.version`) (S).
2. `--dest <name>` on `exec`/`run`, exposed to scripts as `nrg_dest()` plus a
   per-destination secrets file convention
   (`.energize/secrets.staging`) (M).
3. Document the pattern in the authoring guide with a worked example.

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

### 2.4 Runtime decryption of `ENC[...]` + secret-manager adapters — **M**

**Current state:** `nrg secrets encrypt` produces `ENC[...]` tokens, but
nothing decrypts them at runtime — raw ciphertext reaches commands (robustness
review **R3**). And the only sources for `secret()` are env vars,
`.energize/secrets`, and `.env` — all plaintext-on-disk.

**Next steps:**
1. Fix R3: `secret()` transparently decrypts `ENC[...]` values via `.nrg-key`
   (S).
2. Fetch adapters, Kamal-style: resolve `secret("X")` through a configurable
   command (1Password `op read`, Bitwarden, Vault, Doppler) so plaintext never
   lands on disk (M).

### 2.5 Preflight depth for `nrg doctor` — **S**

**Current state:** `doctor` compiles the file and checks *local* tools. Most
first-deploy failures are remote: unreachable host, missing Docker, bad
registry credentials.

**Next steps:** `nrg doctor --hosts` runs the remote preflight (SSH probe,
`docker info`, optional registry auth check) against the hosts the script
declares. Pairs with 1.5.

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

### 2.8 Maintenance mode — **S**

**Current state:** kamal-proxy supports maintenance pages natively; neither
`lib/proxy.rhai` nor `lib/caddy.rhai` exposes it.

**Next steps:** `proxy_maintenance(host, service, on_off, cfg)` in both proxy
backends (same-surface contract), plus `nrg run`-able `maintenance` task in the
examples.

---

## Tier 3 — multipliers

### 3.1 Distribution: prebuilt binaries — **S**

**Current state:** install is "cargo build from source". The audience is app
developers, not Rust developers.

**Next steps:** GitHub Releases with prebuilt binaries (macOS arm64/x86_64,
Linux x86_64/arm64) built by CI on tag, an install script, a Homebrew tap, and
`cargo install nrg` as the fallback. Update README install section.

### 3.2 Embedded stdlib — **M**

**Current state:** `lib/` must be manually vendored (`cp -r lib`) next to every
orchestration file, and never receives updates afterwards — every project
drifts to its own stdlib fork.

**Next steps:** embed `lib/*.rhai` in the binary and resolve
`import "std/docker"` from the embedded copy (version-locked to the binary);
keep `import "lib/…"` for vendored/overridden modules; add `nrg vendor` to
extract the embedded stdlib for customization.

### 3.3 First-class `nrg rollback` — **S**

**Current state:** works as `nrg run rollback`, which requires the script to
wire it up and gives it no discoverability at the panic moment.

**Next steps:** a top-level `nrg rollback [service] [--image tag]` verb (backed
by the stdlib function, using state for defaults), documented prominently.
Depends on 2.3 for rollback-to-any-version.

### 3.4 Templates: `nrg init --template <framework>` — **S**

**Current state:** `nrg init` scaffolds one starter file; framework examples
live in `lib/examples/` and must be copied by hand together with `lib/`.

**Next steps:** `nrg init --template rails|django|nextjs|phoenix|laravel`
writes the corresponding example as `Energize.rhai` (trivial once 3.2 removes
the vendoring step).

### 3.5 Signal handling for the atomic promise — **M**

Robustness review **R7**, listed here because users *perceive* it as a product
promise: Ctrl-C mid-deploy currently runs zero compensations, undermining
"fleet-atomic". Catch SIGINT/SIGTERM, unwind the active transaction, then exit.

---

## Suggested sequencing

1. **Now:** 1.4 `status` ✅ → 2.3 audit log ✅ → 1.2 `logs` ✅ → 1.3 `app exec` ✅
   (small, state-driven, immediately visible; all four shipped), with
   2.4-step-1 (R3 fix) and 3.5 (R7) folded in from the robustness review still
   open.
2. **Next:** 1.1 multi-arch builds → 1.5 `setup` + 2.5 doctor `--hosts` →
   2.1 distributed lock.
3. **Then:** 2.2 destinations → 3.2 embedded stdlib → 3.1 binaries → 3.4
   templates, with 2.6–2.8 slotted in as small wins.

The cut line for a credible `v0.2` announcement is the end of step 2: at that
point a new user on a Mac can bootstrap a fresh VPS and operate the app
day-to-day without leaving `nrg`.
