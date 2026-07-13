# Bunny Magic Containers — Phase 3: Fleet-scale rollout — Implementation Plan

> Read `docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md`'s D3 first. This plan
> implements exactly that: canary-then-batched-parallel upgrade across many Bunny targets, with a
> configurable failure threshold, built on top of Phase 2's `lib/bunny.rhai` primitives
> (`deploy_app`, `wait_for_image`).

**Goal:** `deploy_app` operates on exactly one `(app_id, container)` pair. A real tenant-fleet
upgrade (the motivating use case for this whole roadmap item) needs to push a new image across
potentially hundreds of targets without (a) taking hours doing it strictly sequentially, one
`health_attempts × health_interval` wait at a time, or (b) risking a bad image landing on the whole
fleet before anyone notices it's bad. This phase adds `bunny::deploy_fleet(targets, cfg)`:
canary first (a small slice, verified before anything else is touched), then the REST in
concurrent batches, aborting the whole run if failures exceed a configurable threshold.

**Unlike Phase 2, this phase is NOT constrained to "no new Rust"** — the design spec's phase table
only states that constraint for Phase 2. Genuine wall-clock parallelism across many independent
HTTPS PATCH requests needs real OS-thread concurrency, which Rhai itself cannot express (no
concurrency primitives) — the SAME reason this codebase's SSH path already has `ssh_exec_all` as a
Rust builtin (`std::thread::scope` fan-out) rather than a Rhai-level loop. This phase adds the
direct HTTP analog.

## Task 1: `http_patch_all` — a generic parallel PATCH fan-out builtin

**Files:** `src/engine/builtins/http.rs`

Mirror `ssh_exec_all`'s exact shape and contract (`src/engine/builtins/exec.rs`): given an array of
independent requests, run them **concurrently** via `std::thread::scope`, never abort the batch on
one failure, attribute a thread panic to the right request, and return one `HttpResponse` per
input in the SAME order.

- `http_patch_all(requests: Array) -> Array` where each element is a map
  `#{url: String, body: String, headers?: Map}`. Reject a malformed element (missing `url`/`body`,
  wrong types) with a Rhai-catchable error naming the index, not a panic.
- **Dry-run:** each element short-circuits exactly like a single `http_patch` call would (a
  synthetic 200 + a recorded `check` line) — sequentially, no threads needed (nothing real happens
  regardless). Reuses `write_verb_response`'s existing per-item dry-run branch, just called once
  per array element instead of once total.
- **Live:** `std::thread::scope`, one thread per request, each building its OWN `agent()` (a fresh
  `ureq::Agent` per thread — simpler and just as correct as sharing one, since `agent()` is cheap
  to construct and this avoids any question of whether `ureq::Agent` is safely shared across
  threads) and calling `do_body_request` directly. Join all, attribute a thread panic to that
  request's URL in the response body (mirroring `ssh_exec_all`'s exact panic-attribution comment).
- This is a **generic** HTTP primitive, not Bunny-specific — lives in `http.rs` alongside every
  other verb, matching this codebase's existing separation (`http.rs` knows nothing about Bunny;
  `lib/bunny.rhai` is the only Bunny-aware file).
- Tests: real concurrent round trip against N real local listeners (assert total wall-clock is
  close to the SLOWEST single request, not the SUM — proving genuine concurrency, not a serial
  loop dressed up as one call), per-request failure isolation (one 500 among several 200s doesn't
  lose or corrupt the others), dry-run short-circuits every element with no listener contacted,
  and the malformed-element error path.
- Mutation-verify: temporarily replace the `thread::scope` fan-out with a sequential loop, confirm
  the "close to slowest, not sum" timing test fails, restore, confirm byte-identical via diff.

## Task 2: `bunny::deploy_fleet(targets, cfg)`

**Files:** `lib/bunny.rhai`

```
targets: Array of #{app_id, container, image_tag, image_name?, image_digest?, health_url?}
cfg: #{
    api_key,                  // shared across the whole fleet (one Bunny account)
    canary_size?: 1,          // how many targets go first, sequentially, fully verified
    batch_size?: 5,           // concurrent PATCHes per batch for the REST of the fleet
    max_failures?: 0,         // abort the whole run once this many targets have failed
                              // (canary failures count too — canary_size failures alone abort)
    health_attempts?: 30, health_interval?: 2,   // forwarded to wait_for_image per target
}
```

Returns `#{succeeded: Array<app_id>, failed: Array<#{app_id, error}>}` — never throws for a
per-target failure (that's the whole point of a failure THRESHOLD, not an all-or-nothing throw);
throws only once `failed.len() > cfg.max_failures`, at which point the function stops dispatching
further batches (already-dispatched-and-in-flight work in the CURRENT batch still completes and is
reported, but no NEW batch starts) and the throw message lists every failed target.

**Canary phase (sequential, not batched):** the first `canary_size` targets go through
`deploy_app` + `wait_for_image` ONE AT A TIME (not `http_patch_all` — canary's entire point is
"notice a problem before it's already fanned out", so it must not itself parallelize). Each
canary's health is additionally checked via `health::wait_healthy(target.health_url, ...)` if
`health_url` is present (skipped if absent — Bunny doesn't guarantee a caller has one, e.g. a
non-web background worker). Any canary failure counts toward `max_failures`; if canary alone
already exceeds `max_failures`, throw immediately — the REST of the fleet is never touched.

**Batch phase (parallel):** the remaining targets, `batch_size` at a time, PATCH concurrently via
`http_patch_all` (built directly, not by calling `deploy_app` per target — `deploy_app`'s own
GET+find+PATCH would otherwise mean `batch_size` GETs then `batch_size` PATCHes, all sequential
within the batch; instead this phase does its OWN GET per target — via `app_config`+
`find_container`, still sequential, cheap reads — and only fans the PATCH step out in parallel,
same "one GET one PATCH per target, only the network-bound write is parallelized" shape). Each
target's `.prev` is still snapshotted the same way `deploy_app` does (this phase reuses that exact
snapshot logic, not a copy of it — refactor `deploy_app`'s snapshot step into a small shared
helper if that's the cleanest way to avoid duplicating it). After a batch's PATCHes return,
`wait_for_image` is called per target in that batch (sequential poll, matching Phase 2's existing
per-target polling primitive — polling itself doesn't need to be parallelized, since a caller with
hundreds of targets already accepts batch_size-at-a-time PATCH latency and can raise batch_size).
A batch's own failures count toward `max_failures`; the run stops dispatching new batches once
that threshold is exceeded, but reports every target it got to.

## Task 3: Docs + roadmap

- `docs/stdlib.md`'s `## lib/bunny` section: document `deploy_fleet`, its cfg shape, the
  canary/batch/threshold semantics, and the return shape.
- `docs/roadmap.md`: flip Phase 3 from open to shipped in the 2.9 entry.

## Definition of done

- [ ] `http_patch_all` ships with a genuine concurrency proof test (timing), per-request failure
  isolation, dry-run short-circuiting, and a malformed-element error path — mutation-verified.
- [ ] `bunny::deploy_fleet` does canary-then-batch with a configurable failure threshold, never
  throws for a per-target failure below threshold, and does throw (naming every failure) once
  the threshold is exceeded — without dispatching further batches.
- [ ] Full gate green; docs/roadmap updated.
