# Design: Bunny Magic Containers — Phase 5 (app provisioning + imageTag confirmation)

**Status:** proposed (pending spec review)
**Date:** 2026-07-13
**Author:** Maciek + Claude, from a post-ship feasibility review of roadmap 2.9 (all four original
phases shipped and merged: PRs #55–#58, `lib/bunny.rhai` + the HTTP builtins).

---

## 1. Why this phase exists

Re-reading the shipped module against the actual motivating use case (a multi-tenant SaaS platform
moving its per-tenant hosting to Bunny Magic Containers) surfaced two real gaps, not cosmetic ones:

1. **No provisioning primitive.** Every function in `lib/bunny.rhai` — `deploy_app`, `rollback_app`,
   `wait_for_image`, `deploy_fleet` — requires an **already-existing** `app_id`. There is no
   `bunny::create_app` or `bunny::delete_app`. A platform that wants `nrg` to own a tenant's full
   Bunny lifecycle, not just its image upgrades, still has to reach for Bunny's dashboard or a raw,
   unvalidated API call at onboarding/offboarding time — exactly the kind of gap this module's own
   Phase 1–4 work was meant to close for the upgrade path.
2. **The flagged `imageTag` inference from Phase 2** (see `lib/bunny.rhai`'s header comment) was
   never independently confirmed against a live Bunny account — every doc-page fetch attempted
   during Phase 1/2/4 research returned HTTP 403. This session found a **second, independent, public
   source** that resolves it (below) — worth landing as its own small fix rather than leaving the
   "flagged, unverified" language to rot now that better evidence exists.

## 2. Ground truth (verified this session, not assumed)

Bunny publishes an official Terraform provider, `BunnyWay/terraform-provider-bunnynet`. Its
`internal/api/compute_container_app.go` (fetched directly from GitHub — same "read Bunny's own real
tooling source" method Phase 2 already established, not a guess, not scraped from a 403'ing docs
page) defines the actual API-level Go structs, verbatim:

```go
type ComputeContainerAppContainer struct {
    Id               string `json:"id,omitempty"`
    Name             string `json:"name"`
    PackageId        string `json:"packageId"`
    ImageNamespace   string `json:"imageNamespace"`
    ImageName        string `json:"imageName"`
    ImageTag         string `json:"imageTag"`
    ImageDigest      string `json:"imageDigest"`
    ImageRegistryId  string `json:"imageRegistryId"`
    ImagePullPolicy  string `json:"imagePullPolicy"`
    EntryPoint       ComputeContainerAppContainerEntrypoint `json:"entryPoint"`
    Probes           ComputeContainerAppContainerProbes     `json:"probes"`
    EnvironmentVariables []ComputeContainerAppContainerEnv     `json:"environmentVariables"`
    Endpoints            []ComputeContainerAppContainerEndpoint `json:"endpoints"`
    VolumeMounts         []ComputeContainerAppContainerVolumeMount `json:"volumeMounts"`
}

type ComputeContainerApp struct {
    Id                 string `json:"id"`
    Name               string `json:"name"`
    RuntimeType        string `json:"runtimeType"`
    RegionSettings     ComputeContainerAppRegions      `json:"regionSettings"`
    ContainerTemplates []ComputeContainerAppContainer  `json:"containerTemplates"`
    Volumes            []ComputeContainerAppVolume     `json:"volumes"`
    AutoScaling        ComputeContainerAppAutoscaling  `json:"autoScaling"`
}

type ComputeContainerAppContainerEnv struct {
    Name  string `json:"name"`
    Value string `json:"value"`
}

type ComputeContainerAppVolume struct {
    Name string `json:"name"`
    Size int64  `json:"size"`
}
```

and the REST verbs it drives them with:

- `POST   {apiUrl}/mc/apps`             — create
- `GET    {apiUrl}/mc/apps/{id}`        — read (already used by `app_config`)
- `PUT    {apiUrl}/mc/apps/{id}`        — full-resource update (Terraform's own "replace" semantics)
- `DELETE {apiUrl}/mc/apps/{id}`        — delete

**This corroborates `imageTag`/`imageName`/`imageDigest` exactly** — the same field names
`lib/bunny.rhai` already assumed from the GitHub Action's PATCH body shape alone. Two independent
public sources now agree. That clears this codebase's own "don't guess, ground it in something
real" bar without needing a live Bunny account.

(Source: `github.com/BunnyWay/terraform-provider-bunnynet/blob/main/internal/api/compute_container_app.go`,
a public repository, fetched and read in full this session.)

## 3. Decisions

### D7 — Resolve the `imageTag` inference via corroboration, not a live-account test

The Phase 2 GitHub Action's PATCH body (`{id, imageTag, imageName?, imageDigest?}`) and this
session's Terraform provider struct agree on every field name. Update `lib/bunny.rhai`'s header
comment and `container_image_tag`'s own comment, plus `docs/stdlib.md`'s "flagged inference"
callout, from "flagged, not independently verified" to "corroborated by two independent public
sources" (cite both). This is a comment/doc-only change — no behavior differs, nothing to test.

### D8 — Add `bunny::create_app` / `bunny::delete_app`, built entirely on already-shipped builtins

`POST /mc/apps` and `DELETE /mc/apps/{id}` are both already reachable via Phase 1's `http_post` /
`http_delete` builtins. **Zero new Rust** — matching every prior phase's "zero-vendoring embedded
stdlib" philosophy (roadmap 3.2).

### D9 — `create_app`'s scope: minimal but real, and it extends Phase 4's guardrail to provisioning time

The full `ComputeContainerApp` schema above is large (probes, CDN/sticky-session endpoint config,
multi-container apps, dynamic `allowedRegionIds` vs. static `requiredRegionIds` provisioning,
tunable autoscaling). Scoping to what a per-tenant SaaS app actually needs — one container, one
image, a handful of env vars, an optional single data volume, one pinned region:

```
cfg: #{
    name, api_key,
    image_registry, image_namespace, image_name, image_tag,
    region_id,                 // exactly one required region — see the naming note below
    env?: [#{name, value}, ...],
    volume?: #{name, size, path},   // path is the container's mount path for this volume
    base_url?,
}
```

**`create_app` never exposes an autoscaling knob at all** — every app it creates is provisioned
with `autoScaling: {min: 1, max: 1}` and exactly one required region (`regionSettings:
{requiredRegionIds: [cfg.region_id], allowedRegionIds: []}`), full stop. This isn't a guardrail
bolted onto an otherwise-configurable function — it's the only shape `create_app` can produce,
because messless's own per-tenant model is one process per tenant and Phase 4 already established
*why* a volume-backed Bunny app must never end up multi-replica (an auto-scaled or relocated
replica gets a fresh, empty volume). Provisioning is the one place `nrg` constructs the
`autoScaling`/`regionSettings` blocks itself, so it can simply always get them right rather than
merely rejecting a caller-supplied bad value. Multi-region / multi-replica provisioning is an
explicit non-goal (D11) — not a gap this phase leaves accidentally open.

**Naming note — avoiding a collision with Phase 4's denylist:** `reject_scale_region_keys` already
throws on a literal key named `region` (among others) anywhere on a `deploy_app`/`rollback_app`/
`deploy_fleet` target or cfg map, because those functions have no legitimate use for it. `create_app`
*does* have a legitimate single-region concept, so its cfg key is named `region_id` — a different,
non-colliding string (Rhai map `.contains` checks exact key equality, not substring, so a map
carrying `region_id` does not trip a check for `region`). `create_app`'s cfg is still run through
the same denylist for the *other* forbidden keys (`region`, `regions`, `replica`, `replicas`,
`replica_count`, `scale`, `zone`) — a caller who mistakenly writes `region` (plural-ambiguous with
scaling) instead of the correct `region_id` still gets a loud, named-key refusal instead of silent
misuse, exactly Phase 4's existing behavior, now reused rather than reinvented.

**One more honestly-flagged inference (Fable review will otherwise ask):** neither research source
confirms the exact success status code `POST /mc/apps` returns (200 vs. 201 are both plausible REST
conventions for "created"; nothing fetchable pinned it down). `create_app` accepts either — this is
flagged in the same style as the `imageTag` inference was, not silently assumed.

### D10 — `delete_app`'s scope: a thin, honest DELETE wrapper, nothing cascading

`bunny::delete_app(cfg)`: `cfg: #{app_id, api_key, base_url?}`. Exactly one `DELETE
/mc/apps/<app_id>`, throws on non-2xx. Does not touch DNS records, CDN pull zones, or storage zones
— those stay outside `nrg`'s scope per the original design spec's own non-goals; deleting the
compute app is the one operation actually being asked for here.

### D11 — Non-goals for Phase 5 (keep scope tight)

- **Not** the full `ComputeContainerApp` schema — no probes, no CDN/sticky-session/endpoint config,
  no multi-container apps. A real, deliberately deferred gap; extend if/when an actual use case
  needs it, not speculatively now.
- **Not** app update via the whole-resource `PUT /mc/apps/{id}` the Terraform provider itself uses.
  `deploy_app`'s existing per-container `PATCH` stays the *only* upgrade path — preserving Phase
  2/D5's invariant that a routine upgrade touches only the image reference, never re-declares the
  whole app.
- **Not** multi-region or multi-replica provisioning. `create_app` always provisions exactly one
  region, one replica, full stop (D9).

## 4. Scope of this phase

Single phase, no further sub-phasing — task-level detail lives in
`docs/superpowers/plans/2026-07-13-bunny-phase5-provisioning.md`.

## 5. On the original design spec

`docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md` is now a **merged, historical**
record of a completed, reviewed 4-phase implementation — it is not edited by this phase. This spec
stands alone and cross-references it; the original's phase table (1–4) is left exactly as it
shipped. The original's other non-goals (SSH+Docker untouched, no generic multi-cloud abstraction,
no DNS/CDN/Storage ownership) still hold and are not repeated in full here.
