# Bunny Magic Containers — Phase 4: Volume-pinning guardrails — Implementation Plan

> Read `docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md`'s D4 and D6 first. This
> plan implements exactly those: a structural (not just documented) refusal of any operation that
> could change replica count or region on a volume-backed Bunny target, plus a worked
> dynamic-target-discovery example.

**Context — why this phase looks different from Phases 1-3:** D4 says any scale/region-touching
Bunny operation must refuse by construction. Phases 1-3 deliberately never built a scale/region
operation at all — `deploy_app`/`rollback_app`/`deploy_fleet` only ever read `app_id`, `container`,
`image_tag`, `image_name`, `image_digest`, `health_url`, `base_url` off a target/cfg map, and
`build_patch_request` only ever emits `{id, imageTag, imageName?, imageDigest?}` (see
`lib/bunny.rhai`'s own Opus-review comment on that function — it's a closed, audited set already).
There is no code path today that could silently move replica count or region. Inventing a new
`bunny::scale_app`-style function here would be pure speculation against an API this project has
already twice failed to independently confirm (every Bunny support/doc page fetched during Phase
1/2 research returned HTTP 403) — exactly what `CONTRIBUTING.md` and this project's own established
practice says not to do. So this phase is NOT "add scale/region support, then guard it" — it's
"close the gap where a caller could reasonably (and wrongly) believe passing a scale/region-shaped
key through `nrg` does something, with a loud refusal instead of a silent no-op."

## Task 1: Structural refusal of scale/region-shaped keys on any target/cfg map

**Files:** `lib/bunny.rhai`

Today, a caller who mistakenly writes `#{app_id: ..., container: ..., image_tag: ..., region:
"us-east-1"}` (a natural mistake — many other PaaS APIs DO accept a per-deploy region) gets no
error and no effect: `region` is simply never read. Silence here is exactly the failure mode D4
warns about — "just a doc warning" is not good enough, and a *silent no-op* is worse than a doc
warning, since it looks like success. Fix: reject the map outright, by name, everywhere a
target/cfg map is accepted.

- A small denylist of forbidden keys: `region`, `replicas`, `replica_count`, `scale`, `zone` (the
  shapes a caller could plausibly reach for when trying to affect where/how many copies of a
  volume-backed app run — matching D4's own wording, "replica count or region").
- A shared `private fn reject_scale_region_keys(m, what)` helper (`what` names the map for the
  error, e.g. `"target"` or `"cfg"`) — throws `"bunny::<what>: refusing to accept <key> — nrg does
  not support changing a Bunny app's replica count or region; Bunny volumes are pinned per-replica
  and an auto-scaled or relocated replica gets a fresh, empty volume. Use Bunny's own dashboard/API
  for this, deliberately outside nrg's scope."` naming the actual offending key.
- Call it from every entry point that accepts a raw target/cfg map: `deploy_app(cfg)`,
  `rollback_app(cfg)`, and `deploy_fleet`'s per-target `validate_target` (extend the existing
  loop, don't duplicate it) AND once on the shared fleet `cfg` itself (a caller could put `region`
  on the shared cfg instead of a target — same mistake, same refusal).
- This must NOT reject `base_url` (already a legitimate, documented override) or any of the
  existing known-good keys — a decoy test with a fully valid target/cfg map must keep passing
  unmodified.

## Task 2: Worked dynamic-target-discovery example

**Files:** `docs/stdlib.md`

Per D6, this is a documented pattern, not new engine capability — `http_get` already exists,
Rhai is already a real scripting language, and a script can already build the `targets` array
dynamically before calling `deploy_fleet`. Add a worked example under the `lib/bunny` section
showing: an external tenant-registry `http_get`, `from_json` to parse it, a Rhai `for` loop
building `#{app_id, container, image_tag}` maps from the registry response, and passing the
resulting array straight to `deploy_fleet`. No new code — a doc-only deliverable, exactly as D6
decided.

## Task 3: Docs + roadmap

- `docs/stdlib.md`'s `## lib/bunny` section: document the denylist and its refusal message
  alongside the dynamic-target-discovery example.
- `docs/roadmap.md`: flip Phase 4 from open to shipped in the 2.9 entry — this closes out roadmap
  2.9 entirely (all four phases shipped).

## Definition of done

- [ ] `deploy_app`, `rollback_app`, and `deploy_fleet` (both per-target and shared-cfg) all refuse
  a `region`/`replicas`/`replica_count`/`scale`/`zone` key with a clear, named-key error — before
  any network call.
- [ ] A fully valid target/cfg map (no forbidden keys) is provably unaffected — existing Phase
  1-3 tests keep passing unmodified.
- [ ] Mutation-verified: temporarily remove the guardrail's call site, confirm the new regression
  test(s) fail for the right reason, restore, confirm byte-identical via diff.
- [ ] Worked dynamic-target-discovery example added to `docs/stdlib.md`.
- [ ] Full gate green; docs/roadmap updated (roadmap 2.9 fully shipped, all four phases).
