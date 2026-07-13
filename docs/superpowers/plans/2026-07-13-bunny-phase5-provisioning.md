# Bunny Magic Containers — Phase 5: App provisioning + imageTag confirmation — Implementation Plan

> Read `docs/superpowers/specs/2026-07-13-bunny-provisioning-design.md` first — this plan
> implements D7–D11: resolving the Phase 2 flagged `imageTag` inference via a second independent
> public source (no code behavior change), and adding `bunny::create_app` / `bunny::delete_app` so
> `nrg` can own a Bunny tenant's full lifecycle, not just its image upgrades.

## Task 1: Resolve the flagged `imageTag` inference (comment/docs only, no behavior change)

**Files:** `lib/bunny.rhai`, `docs/stdlib.md`

- `lib/bunny.rhai`'s header comment (currently "ONE STRUCTURAL INFERENCE, FLAGGED, NOT
  INDEPENDENTLY VERIFIED...") and `container_image_tag`'s own comment: replace with a note that
  this is now corroborated by **two** independent public sources — the GitHub Action's PATCH body
  shape (Phase 2) **and** Bunny's own Terraform provider Go source
  (`BunnyWay/terraform-provider-bunnynet/internal/api/compute_container_app.go`'s
  `ComputeContainerAppContainer.ImageTag string \`json:"imageTag"\``), both quoted in the Phase 5
  design spec's ground-truth section. Downgrade the language from "flagged, unverified" to
  "corroborated by two independent sources, not live-account-tested" — don't overclaim certainty
  that doesn't exist either.
- `docs/stdlib.md`'s module-level "Ground truth" callout and the `current_image_tag` section: same
  update, same two-source citation.
- No test changes — this task changes no behavior. Verify by re-reading the diff; nothing here is
  mutation-testable.

## Task 2: `create_app(cfg) -> map`

**Files:** `lib/bunny.rhai`, `tests/bunny.rs`, `docs/stdlib.md`

`cfg: #{name, api_key, image_registry, image_namespace, image_name, image_tag, region_id, env?:
[#{name, value}, ...], volume?: #{name, size, path}, base_url?}`.

Test-first, in this order:

1. `create_app_requires_every_mandatory_key_before_contacting_anything` — missing any of `name`,
   `image_registry`, `image_namespace`, `image_name`, `image_tag`, `region_id` throws a clear error
   naming the specific missing key, **before** any network call (mirror `deploy_fleet`'s
   `validate_target` style exactly).
2. `create_app_refuses_a_denylisted_key_before_contacting_anything` — `cfg` containing a literal
   `region`/`regions`/`replica`/`replicas`/`replica_count`/`scale`/`zone` key throws via the
   existing `reject_scale_region_keys("bunny::create_app")` — reused, not reinvented. A decoy test
   confirms a cfg using the *correct* `region_id` key is unaffected (no false-positive collision —
   `.contains("region")` is false for a map that only has `region_id`).
3. `create_app_posts_the_expected_body_and_returns_the_new_app` — a mock `POST /mc/apps` listener
   captures the request body and asserts the full JSON shape: `name`, `regionSettings:
   {requiredRegionIds: [region_id], allowedRegionIds: []}`, `autoScaling: {min: 1, max: 1}`,
   `containerTemplates[0]` with `imageRegistryId`/`imageNamespace`/`imageName`/`imageTag`/
   `environmentVariables` (from `cfg.env`, `[]` if absent), and (if `cfg.volume` present)
   top-level `volumes: [{name, size}]` plus the container's `volumeMounts: [{name, path}]`. Asserts
   the auth header is `AccessKey` (same as every other call in this module), and that the function
   returns `from_json(r.body)` (the created app, including its new `id`) so a caller can capture
   `app_id` for later `deploy_app`/`deploy_fleet` calls.
4. `create_app_forces_single_replica_and_single_region_regardless_of_volume` — confirm there is no
   cfg key that can change `autoScaling`/`regionSettings` away from the fixed shape (D9: `create_app`
   never reads an autoscaling override off `cfg` at all — this isn't conditional on `cfg.volume`,
   it's unconditional).
5. `create_app_accepts_either_200_or_201_as_success` — table-test both statuses; anything else
   throws (mirroring the flagged-but-honest status-code inference in D9).
6. `create_app_under_dry_run_never_makes_a_real_post` — inherited from `http_post`'s own dry-run
   short-circuit (Phase 1), but write an explicit test locking it in for this new entry point —
   matching this codebase's own practice of testing dry-run per new entry point even when the
   underlying primitive already handles it (see `wait_for_image`'s own explicit dry-run test).

Implementation notes:

- Build the request body via `to_json(...)` of the exact map shape above; POST to
  `base_url(cfg) + "/mc/apps"` with `auth_headers(cfg.api_key)`.
- A single container template only (`name: "web"` — no multi-container support, per D11).
- `env`/`volume` are optional — omit `volumeMounts`/`volumes` entirely when `cfg.volume` is absent,
  rather than emitting empty-but-present keys (matches `build_patch_request`'s existing
  omit-when-absent convention for `imageName`/`imageDigest`).

## Task 3: `delete_app(cfg) -> HttpResponse`

**Files:** `lib/bunny.rhai`, `tests/bunny.rs`, `docs/stdlib.md`

`cfg: #{app_id, api_key, base_url?}`. One `DELETE /mc/apps/<app_id>` via `http_delete`, throws on
non-2xx.

- Rename `patch_failure_detail` → `transport_failure_detail` (its logic only ever inspected
  `r.status`/`r.body` generically — nothing PATCH-specific — and Phase 5 gives it a second caller;
  a name that only mentions PATCH is misleading once `delete_app` also uses it). Update both call
  sites (`patch_container`'s existing use, `delete_app`'s new use) and any doc reference in
  `docs/stdlib.md`/`docs/builtins.md`.
- Tests: `delete_app_sends_a_real_delete_and_returns_success`,
  `delete_app_throws_a_clear_error_on_non_2xx_with_transport_detail_on_status_zero`,
  `delete_app_under_dry_run_never_makes_a_real_delete`.

## Task 4: Full gate + mutation-verify

- `cargo build --all-targets --locked && cargo clippy --all-targets --locked -- -D warnings &&
  CI=true cargo test --all-targets --locked`.
- Mutation pass: temporarily hardcode `create_app`'s `autoScaling`/`regionSettings` block to pass
  through a caller-supplied override instead of the fixed shape; confirm
  `create_app_forces_single_replica_and_single_region_regardless_of_volume` fails for the right
  reason; restore; confirm the diff is byte-identical to before the mutation.

## Task 5: Docs + roadmap

- `docs/stdlib.md`: add `create_app`/`delete_app` sections under `## lib/bunny`; update the
  "Ground truth" callout and `current_image_tag` section per Task 1.
- `docs/roadmap.md`: add a "Phase 5" line under the existing 2.9 entry — provisioning primitives
  (`create_app`/`delete_app`) plus the `imageTag` inference resolved via a second independent
  public source. The existing four-phase "✅ shipped (all four phases)" framing stays intact —
  Phase 5 is additive, found during a post-ship feasibility review, not a re-opening of the
  original phase table.
- Do **not** edit `docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md` — it's a
  merged historical record (see the Phase 5 design spec's §5).

## Definition of done

- [ ] `imageTag` inference language updated in both `lib/bunny.rhai` and `docs/stdlib.md`, citing
  the two independent sources; no behavior change.
- [ ] `bunny::create_app` implemented and tested: mandatory-key validation, denylist reuse (+ a
  `region_id`-doesn't-collide decoy test), full request-body shape, forced single-replica/
  single-region, either-status-code acceptance, dry-run.
- [ ] `bunny::delete_app` implemented and tested: happy path, non-2xx error with transport detail,
  dry-run.
- [ ] `patch_failure_detail` renamed to `transport_failure_detail`, both call sites + doc
  references updated.
- [ ] Full `CONTRIBUTING.md` gate green.
- [ ] Mutation-verified per Task 4.
- [ ] `docs/stdlib.md` and `docs/roadmap.md` updated.
