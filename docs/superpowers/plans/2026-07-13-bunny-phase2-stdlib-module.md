# Bunny Magic Containers — Phase 2: `lib/bunny.rhai` stdlib module — Implementation Plan

> **For agentic workers:** Read `docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md`
> first — this plan implements exactly the Phase 2 row of that spec's phase table (single-target
> image upgrade, status poll, rollback-to-previous-tag), built entirely on Phase 1's HTTP primitives
> (already shipped: `http_get`/`http_patch` with headers, see
> `docs/superpowers/plans/2026-07-12-bunny-phase1-http-client.md`). Phases 3–4 (fleet-scale rollout,
> volume guardrails) are follow-on plans, not part of this one.

**Goal:** give a project a Rhai-only way to drive a Bunny Magic Containers app (`bunny::deploy_app`
for a single-target image upgrade, `bunny::rollback_app` to revert to the previously-deployed tag,
`bunny::wait_for_image`/`bunny::current_image_tag` to poll deploy status) — zero new Rust engine
code, matching the "ssh_exec is a primitive, deploy() is stdlib built on it" layering this codebase
already uses for the SSH+Docker path.

**Ground truth for Bunny's actual REST API (verified 2026-07-13, not assumed):** the design spec
deliberately left Bunny's exact endpoint shape unspecified pending research. That research is now
done — fetched and read in full, verbatim, from Bunny's own **public, official** GitHub Action
source (`BunnyWay/actions/container-update-image/src/action.ts`, the action Bunny's own docs tell
users to wire into CI for automated image updates):

```typescript
// GET https://api.bunny.net/mc/apps/{appId}
//   headers: { Accept: application/json, AccessKey: <api_key> }
//   -> 200: { id, containerTemplates: [ { id, name, ... }, ... ] }
//   -> 400: bad app_id
//   -> anything else non-200: generic HTTP failure

// PATCH https://api.bunny.net/mc/apps/{appId}/containers/{containerId}
//   headers: { Content-Type: application/json, AccessKey: <api_key> }
//   body: { id: containerId, imageTag, imageName?, imageDigest? }
//   -> 200: accepted; anything else: failure
```

Key facts this plan relies on, all read directly from that source:
- Auth is a single `AccessKey: <key>` header — **not** `Authorization: Bearer`.
- A container is identified by **name** in `cfg` (matching the GitHub Action's own `container`
  input) but the API itself addresses it by **id** — the real action GETs the app, filters
  `containerTemplates` by `.name === containerName`, throws if it finds zero or more than one match,
  then PATCHes using the matched `.id`. This plan reproduces that exact lookup and both exact error
  messages ("Could not find container named...", "Found more than one container named...") since
  they're the real, already-shipped, user-facing strings from Bunny's own tooling — no reason to
  invent different wording for the same failure.
- The exact per-status-code error messages above are quoted from the real source, not invented.

**One structural inference, flagged, not verified against a live account:** the GET response's
`containerTemplates[i]` TypeScript type in the fetched source only *declares* `id`/`name` (the two
fields that one action happens to use) — it doesn't rule out (or confirm) an `imageTag` field
alongside them. Given the PATCH body's shape is literally `{id, imageTag, imageName?, imageDigest?}`
— i.e. exactly what one `containerTemplates` entry needs to describe — it is a reasonable, but
**not independently confirmed**, inference that GET's `containerTemplates[i].imageTag` reflects the
currently-deployed tag. This assumption is isolated to exactly one function
(`bunny::current_image_tag`, used by `wait_for_image` and by `rollback_app`'s snapshot step) and
called out again at that function's definition — a maintainer with a real Bunny account should
verify this once and delete the caveat (or fix the field name) rather than this plan guessing
further from outside documentation that returned HTTP 403 to every fetch attempt during this
research pass.

**Architecture:** new `lib/bunny.rhai`, following `lib/notify.rhai`'s precedent for "thin Rhai
wrapper over `http_*` builtins, zero new Rust" and `lib/healthcheck.rhai`'s precedent for
retry-loop-with-`cfg` polling helpers. Reuses, unmodified:
- `http_get`/`http_patch` (Phase 1) — headers overload carries `AccessKey`; GET's already-honest
  dry-run semantics (a real read of existing reality, per `docs/builtins.md`) means an app-config
  fetch is truthful even under `--dry-run`; PATCH's already-short-circuiting dry-run semantics (a
  synthetic `200` + recorded `check` action) means the actual image-update call needs **zero** new
  dry-run plumbing in this module — it falls out of Phase 1 for free, exactly as the design spec
  predicted ("no new Rust needed for this phase").
- `from_json`/`to_json` (already shipped) to parse/build the app-config and PATCH-body JSON.
- `state_set`/`state_get`/`has_state` (already shipped) for the `.prev`-tag rollback snapshot,
  namespaced `"bunny." + app_id + "." + container + ".prev"` — same dotted-key convention
  `lib/deploy.rhai` already uses for `<service>.prev`.
- `secret()`/`reveal()`/`type_of(...) == "Secret"` (already shipped) so `cfg.api_key` may be a bare
  `secret("BUNNY_API_KEY")` — mirroring `lib/notify.rhai::webhook`'s exact `Secret`-or-plain-string
  handling for its own `url` argument, since a Bunny API key is exactly the same shape of
  credential-that-might-be-a-Secret.
- `is_dry_run()` (already shipped, used by `lib/healthcheck.rhai::ssh_http_status`) so
  `wait_for_image`'s polling loop can short-circuit under `--dry-run` — there's no Rust-level "sim"
  backend for a Bunny app (unlike a container's `sim_container_healthy`), so the not-actually-changed
  remote state under dry-run needs the SAME kind of explicit `is_dry_run()` escape hatch
  `ssh_http_status` already established for the analogous "can't observe a not-really-changed remote
  thing under dry-run" problem.

---

## Why these files

| File | Responsibility |
|---|---|
| `lib/bunny.rhai` | The whole module: `app_config`, `find_container`, `current_image_tag`, `deploy_app`, `rollback_app`, `wait_for_image`. |
| `tests/bunny.rs` | New integration test file, following `tests/lifecycle_hooks.rs`'s exact pattern (`link_lib`/`run` helpers, a real local TCP listener for live-request assertions, `--dry-run` assertions against the recorded plan text). |
| `docs/builtins.md` or a new `docs/bunny.md` | Document the module's public functions, `cfg` shape, and the one flagged inference above. |
| `docs/roadmap.md` | Flip "2.9 PaaS provider targets" Phase 2 from open to shipped. |

**Deliberate deferral (documented, not silent):** this plan does **not** add fleet-scale/batched
rollout (Phase 3) or volume-pinning guardrails (Phase 4) — `deploy_app`/`rollback_app` operate on
exactly one `(app_id, container)` pair per call, matching the design spec's D5 scope constraint
("a mass-upgrade operation touches only the image reference... set once at provision time and left
alone"). A caller wanting fleet-wide upgrades loops over its own tenant list calling `deploy_app`
once per tenant — Phase 3 is specifically about replacing that naive sequential loop with
canary+batched parallelism, not part of this plan.

---

## Task 1: Implement `lib/bunny.rhai`

**Files:** Create `lib/bunny.rhai`

- [ ] **Step 1: `app_config(cfg)` — GET the app's full config**

  `cfg: #{app_id, api_key}` (`api_key` may be a `Secret` or plain string). Reveals `api_key` if it's
  a `Secret`, GETs `https://api.bunny.net/mc/apps/<app_id>` with
  `#{"AccessKey": key, "Accept": "application/json"}`, `from_json`s the body. Reproduce the real
  action's exact status handling: `status == 400` → `"Could not obtain app configuration: double- \
  check cfg.app_id (got a 400 from Bunny)."`; any other non-200 → `"Could not obtain app \
  configuration: HTTP status <status>."` (mirroring, not verbatim-copying, the real action's
  wording — this codebase's own convention is full sentences ending in a period, see
  `lib/healthcheck.rhai`'s throws).

- [ ] **Step 2: `find_container(app_config, container_name)` — locate a container by name**

  Iterate `app_config.containerTemplates`, filter by `.name == container_name`. Zero matches →
  throw naming the container and app; more than one match → throw naming the container (both
  mirroring the real action's own two distinct error cases). Returns the matched container map
  (so callers get `.id` and whatever else Bunny returned, not just the id alone).

- [ ] **Step 3: `current_image_tag(cfg)` — convenience read**

  `cfg: #{app_id, api_key, container}`. Calls `app_config` + `find_container`, returns
  `.imageTag` off the match. **Carries the Task-header's flagged inference** — say so again in this
  function's own doc comment, one sentence, not a wall of caveats repeated everywhere.

- [ ] **Step 4: `deploy_app(cfg)` — the actual image upgrade**

  `cfg: #{app_id, api_key, container, image_tag, image_name?, image_digest?}`. Steps: `app_config` →
  `find_container` → snapshot the match's CURRENT `.imageTag` into
  `state_set("bunny." + app_id + "." + container + ".prev", <current tag>)` (skip the snapshot with
  a one-line `print` note if the current tag is already unit/absent — don't `state_set` a `()` as a
  string) → PATCH `https://api.bunny.net/mc/apps/<app_id>/containers/<container_id>` with body
  `#{id: container_id, imageTag: image_tag}` (plus `imageName`/`imageDigest` if present in `cfg`) via
  `to_json` + `http_patch(url, body, #{"AccessKey": key})` — no explicit `Content-Type` needed,
  `http_patch`'s own default applies. Non-200 (live) → throw naming the real status, mirroring the
  real action's own message. Returns the `HttpResponse`. **No explicit dry-run branch anywhere in
  this function** — the GET is honest, the PATCH already short-circuits, by construction.

- [ ] **Step 5: `rollback_app(cfg)` — revert to the previously-deployed tag**

  `cfg: #{app_id, api_key, container}`. Reads `"bunny." + app_id + "." + container + ".prev"` via
  `state_get`; throws a clear "no previous image recorded for this app/container — nothing to roll
  back to" if absent (`state_get` returns `()` on a miss, per `docs/builtins.md`'s documented
  gotcha — test with `!= ()`, not truthiness). Calls the SAME low-level PATCH path `deploy_app` uses
  — but must **not** re-snapshot `.prev` to the value it's rolling back FROM, matching
  `lib/deploy.rhai`'s own single-snapshot convention (`docs/roadmap.md`'s documented "rollback only
  ever targets the single snapshotted `.prev`" limitation) — refactor the shared GET+find+PATCH
  logic into a private helper both `deploy_app` and `rollback_app` call, with only `deploy_app`
  doing the snapshot step.

- [ ] **Step 6: `wait_for_image(cfg)` — poll until the deploy has propagated**

  `cfg: #{app_id, api_key, container, image_tag, attempts?: 30, interval?: 2}`. Mirror
  `lib/healthcheck.rhai::wait_port`'s retry-loop shape (same `attempts < 1` guard convention,
  robustness review R26). **Dry-run:** `is_dry_run()` short-circuits to "assumed propagated"
  immediately, no polling, no real GET — there is no live change to observe under dry-run (PATCH
  never ran), exactly the same reasoning `ssh_http_status` already documents for its own
  not-yet-real container. **Live:** loop calling `current_image_tag(cfg)`, comparing to
  `cfg.image_tag`, sleeping `interval` between attempts; throws after exhaustion naming the last
  observed tag. 2-arg overload (`app_id`, `container` folded into one `cfg` map — no separate
  overload needed beyond the `attempts`/`interval` defaults already handled inside the function,
  matching `wait_healthy`'s single-`cfg`-arg style rather than `wait_port`'s split-args style, since
  this function already has four required identity fields — `app_id`/`api_key`/`container`/
  `image_tag` — where positional args would be error-prone to call correctly).

---

## Task 2: Tests

**Files:** Create `tests/bunny.rs`

Follow `tests/lifecycle_hooks.rs`'s exact pattern (`link_lib`, a real local `TcpListener` for
live-request assertions, `--dry-run` assertions against the recorded plan/print text). Do **not**
mock `ureq` — this codebase's own convention (established in `src/engine/builtins/http.rs`'s own
tests) is a real loopback listener.

- [ ] A live `deploy_app` call: spin up a listener that answers the GET with a fixed
  `containerTemplates` JSON body (one matching container, one non-matching, to prove the filter
  works), then answers the PATCH, asserting the PATCH request's raw bytes contain the right
  `AccessKey` header value and the right `imageTag` in the JSON body, and that the request path is
  `/mc/apps/<id>/containers/<container-id>` with the CORRECT container id (the matched one, not the
  app id).
- [ ] `deploy_app` throws a clear error when zero containers match the name.
- [ ] `deploy_app` throws a clear error when more than one container matches the name.
- [ ] `deploy_app` throws naming the real HTTP status on a non-200 GET, and again on a non-200 PATCH.
- [ ] `deploy_app` snapshots the previous tag into state (assert via a following `state_get` in a
  second `nrg exec`, same persisted-directory pattern
  `a_blocking_hook_pre_deploy_does_not_corrupt_the_rollback_chain` already uses).
- [ ] `rollback_app` reads the snapshotted tag and PATCHes back to it; throws a clear error when no
  `.prev` is recorded yet.
- [ ] `rollback_app` does NOT re-snapshot `.prev` (two sequential deploy_app/rollback_app calls,
  assert `.prev` still holds the original pre-deploy tag, not the value just rolled back from).
- [ ] Live `deploy_app`/`rollback_app` never touch the real network under `--dry-run`: assert the
  dry-run PLAN records the expected `check`/`state` lines and that no listener ever receives a
  connection (bind a listener, assert `try_recv`/`recv_timeout` times out).
- [ ] `wait_for_image`: a live loop that succeeds once `current_image_tag` matches (a listener that
  returns the old tag N times then the new tag), and one that exhausts `attempts` and throws naming
  the last-seen tag. A `--dry-run` call returns immediately with no listener interaction at all.
- [ ] `api_key` accepted as either a bare `secret(...)` or a plain string (mirror
  `notify_webhook_accepts_a_secret_url_and_reveals_it_before_posting`).

- [ ] **Mutation-verify** (per `CONTRIBUTING.md`): temporarily break the `is_dry_run()` short-circuit
  in `wait_for_image`, confirm its dry-run test now fails (would try a real GET against nothing),
  restore, confirm byte-identical via diff.

---

## Task 3: `cargo clippy` + the full local gate

```bash
cargo build --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
CI=true cargo test --all-targets --locked
```

(Rhai files aren't compiled by `cargo`, but the new `tests/bunny.rs` is — this gate still applies in
full, plus running a quick standalone `nrg exec` against a scratch project as a smoke check that
`lib/bunny.rhai` itself parses/imports cleanly, since a Rhai syntax error inside the module wouldn't
be caught by `cargo build` at all.)

---

## Task 4: Update the living docs

- [ ] Add a `## Bunny Magic Containers (lib/bunny.rhai)` section to `docs/builtins.md` (or a new
  `docs/bunny.md` linked from `docs/README.md`'s index, whichever this codebase's existing doc
  structure favors once Task 1 is done and the actual function list is final) documenting every
  public function, its `cfg` shape, the dry-run semantics (inherited from Phase 1, explicitly called
  out as "nothing new here"), and the one flagged `imageTag`-field inference.
- [ ] Flip `docs/roadmap.md`'s "2.9 PaaS provider targets" Phase 2 line from open to shipped, in the
  same style Phase 1's entry already uses.
- [ ] Commit.

---

## Definition of done

- [ ] `lib/bunny.rhai` implements `app_config`/`find_container`/`current_image_tag`/`deploy_app`/
  `rollback_app`/`wait_for_image`, built entirely on already-shipped builtins — no Rust changes.
- [ ] Every claim about Bunny's real API shape in this module's doc comments is either (a) traced to
  the verified `BunnyWay/actions` source quoted above, or (b) explicitly flagged as an inference,
  not silently presented as fact.
- [ ] New tests cover the container-name lookup (0/1/>1 matches), the prev-tag snapshot/rollback
  round trip (including "rollback doesn't re-snapshot"), dry-run short-circuiting for both the write
  path and the poll path, and the `Secret`-or-plain-string `api_key` argument.
- [ ] `cargo build --all-targets --locked && cargo clippy --all-targets --locked -- -D warnings &&
  CI=true cargo test --all-targets --locked` all pass clean.
- [ ] `docs/builtins.md` (or `docs/bunny.md`) and `docs/roadmap.md` reflect the new module.
- [ ] A follow-up session can start Phase 3 (fleet-scale rollout) using `deploy_app`/`wait_for_image`
  as the per-target primitive to loop/batch over — no further Rust or Phase-2 changes required.
