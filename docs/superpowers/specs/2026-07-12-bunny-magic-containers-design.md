# Design: Bunny Magic Containers as an `nrg` deploy target

**Status:** proposed (pending spec review)
**Date:** 2026-07-12
**Author:** Maciek + Claude (brainstorming from a real consumer use case — a multi-tenant SaaS
platform deploying one container per merchant to Bunny Magic Containers, needing fleet-wide image
upgrades across potentially hundreds of tenant apps)

---

## 1. Goal

Today `nrg` deploys to a **list of SSH-reachable hosts running Docker** — the entire engine
(`CommandRunner::run_ssh`, `lib/deploy.rhai`, `lib/proxy.rhai`) is built on that one primitive.
**Bunny Magic Containers is a managed container PaaS with no SSH surface at all** — you drive it
through a REST API (push an image, the platform runs it across regions, terminates TLS, manages
volumes). This spec scopes what `nrg` needs to treat "a Bunny Magic Containers app" as a first-class
deploy target, at **fleet scale** — the real motivating case is a SaaS control plane doing a
coordinated image upgrade across every tenant's own container, not a single app deploy.

This is deliberately scoped as an **addition**, not a rewrite: the SSH+Docker path is this tool's
proven, well-tested core (see `docs/robustness-review.md`) and must not regress. A Bunny target is
a second, parallel capability.

## 2. What's already reusable (verified against the real source, not assumed)

- **Multi-arch build/push** (`lib/deploy.rhai`'s `cfg.platform`, roadmap 1.1) already produces
  `linux/amd64` images — exactly what Bunny requires. The build side needs nothing new.
- **`to_json`/`from_json`** (`src/engine/builtins/util.rs`) already round-trip a Rhai
  map/array/string/int/bool to/from a JSON string via `serde_json`. A Bunny API's JSON request/
  response bodies are already fully representable in Rhai today — this is a solved problem, not a
  gap.
- **`http_get`** (`src/engine/builtins/http.rs`) already does real GETs, with an honest dry-run
  short-circuit (`sim_http_healthy` for a not-yet-live target, a real probe for `http_get` itself)
  — this is the right shape for polling a deployed tenant's public health endpoint
  (`https://<sub>.<base>/healthz`) exactly the way SSH deploys already health-gate today.
- **The fleet-atomic orchestration shape** in `lib/deploy.rhai` (sequential roll, health-gate before
  cutover, whole-fleet rollback on a mid-roll failure) is conceptually correct for "push a new image
  across N tenant containers, abort cleanly if one fails." It does not need reinventing — only a
  new low-level primitive to roll onto.
- **Encrypted secrets** (`age`-based, `src/secrets/`), **the deploy lock**, **dry-run simulation**,
  **audit trail**, **multi-environment destinations** — all engine-level, provider-agnostic.
  Rollback-to-a-snapshotted-previous-tag is likewise a concept, not an SSH-specific mechanism.

## 3. What's actually missing

### D1 — Transport: `CommandRunner` cannot represent Bunny at all

**Verified in `src/engine/runner.rs`:** `CommandRunner` is exactly `run_ssh` / `run_local` /
`run_ssh_stdin` / `run_local_stdin` — every method assumes "spawn a command against an SSH host or
locally." Bunny's API is not "run an arbitrary shell command," it's a fixed set of REST operations
(update an app's image, set an env var, read deploy status, scale replicas). **Decision: do not try
to shoehorn Bunny into `CommandRunner`.** It gets its own builtin family, parallel to (not inside)
the SSH runner — the same way `http.rs` is already a separate builtin module from `exec.rs`, not a
`CommandRunner` impl.

### D2 — The HTTP builtins are too narrow to drive a real authenticated REST API

**Verified in `src/engine/builtins/http.rs`:** `http_get(url)` and `http_post(url, body)` exist.
Neither accepts custom headers (no way to send `Authorization: Bearer <key>`), there is no
`http_put`/`http_patch`/`http_delete`, and `http_post` hardcodes `Content-Type: application/json`.
This is sufficient for a health-check GET or a fire-and-forget webhook (`notify::webhook`) but not
for a real CRUD-shaped provider API.

**Decision: level up the HTTP builtins first** (headers + more verbs), and build the Bunny provider
**entirely in Rhai stdlib** (`lib/bunny.rhai`) on top of the leveled-up primitive — consistent with
this codebase's own "zero-vendoring embedded stdlib" philosophy (roadmap 3.2) and its
"`ssh_exec` is a primitive, `deploy()` is stdlib built on it" layering. This is the **smallest,
highest-leverage, independently shippable first phase** — see the Phase 1 plan.

### D3 — Sequential-only rollout does not scale to a real tenant fleet

`deploy()` rolls hosts **sequentially** (`docs/deploy.md`: "Rolled sequentially"). Fine for a
handful of SSH boxes; a fleet-wide upgrade across hundreds of tenant Bunny apps, each waiting out a
`health_attempts × health_interval` poll one at a time, could take hours. **Decision:** mass
deployment needs a genuinely new capability — canary-then-batched rollout (deploy to a small
canary slice first, verify, then N-at-a-time parallel batches, abort the whole run past a
configurable failure threshold). This is new orchestration logic, not a retarget of existing code —
scoped to its own phase (Phase 3), after the Bunny primitive itself exists.

### D4 — Bunny's per-replica volume pinning needs structural guardrails, not just documentation

A volume-backed Bunny app must stay pinned to **one replica** (an auto-scaled second replica gets a
fresh, empty volume — verified against the real deploy target's own operational docs, which already
carry this warning for a single-app case). A tool that can trigger mass operations across many
tenant apps needs to **refuse**, not just warn about, any operation that would change replica
count or region on a volume-backed target. Scoped to Phase 4, alongside the provider's scale/region
operations (the earliest phases don't touch replica count at all, so this isn't a blocker for
Phase 1–2).

### D5 — Per-tenant config divergence is a non-problem, if scoped correctly

Each tenant app needs its own env (a per-tenant secret, subdomain, data path) even though every
tenant runs the *same* image. **Decision:** a mass-upgrade operation touches **only the image
reference**, never env/volume config, which is set once at provision time and left alone on every
subsequent upgrade. This is a scope constraint the Bunny provider module should enforce by
construction (its "upgrade" function takes an image ref and a target list, nothing else) rather than
something requiring new machinery.

### D6 — Dynamic target discovery is probably already possible, not a gap

`nrg`'s target lists are literal Rhai arrays today, but Rhai is a real scripting language with
`http_get` already available — a script can call an external tenant-registry API and build the
target array at runtime before calling a deploy function. **Decision: treat this as a documented
pattern (an example script), not a new engine capability**, until real use proves otherwise.

## 4. Phases

| Phase | Scope | Depends on |
|---|---|---|
| **1** | HTTP builtin: headers + `http_put`/`http_patch`/`http_delete` | — |
| **2** | `lib/bunny.rhai` stdlib module: single-target image upgrade (`bunny::deploy_app(app_id, image, cfg)`), status poll, rollback-to-previous-tag | Phase 1 |
| **3** | Fleet-scale rollout: canary + batched parallel upgrade across many targets, configurable failure threshold | Phase 2 |
| **4** | Volume-pinning guardrails on any scale/region-touching Bunny operation; a worked dynamic-target-discovery example | Phase 2 |

Phase 1 is scoped in full below (`docs/superpowers/plans/2026-07-12-bunny-phase1-http-client.md`).
Phases 2–4 are intentionally left at the decision level above, not pre-written step-by-step — this
codebase's own precedent (the Rhai migration's five phase files) shows each phase plan gets written
once the prior phase has actually landed and its real shape is known, not spec'd blind up front.

## 5. Non-goals

- Replacing or modifying the SSH+Docker path. Bunny is additive.
- A generic "any cloud provider" abstraction layer. Scope this to Bunny's actual API shape; a
  second provider (Fly, Render, …) is a future spec, not a speculative interface designed now.
- Bunny-side features `nrg` has no business owning: DNS, CDN cache rules, Storage-zone management.
  Only the container/app lifecycle (image, env, status, scale) is in scope.
