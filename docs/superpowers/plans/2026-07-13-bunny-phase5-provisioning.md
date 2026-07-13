# Bunny Magic Containers — Phase 5: Provisioning — Research Findings

> **Correction (Opus review round):** an earlier version of this document concluded ground truth
> for app creation/deletion could not be found anywhere and declined to implement Phase 5 on that
> basis. That conclusion was **wrong** — a real, checkable source exists (below) and was missed
> because only a rendered doc-site mirror was checked, not that same project's actual source code.
> The corrected conclusion is different: ground truth **does** exist, but implementing full app
> provisioning against it is a substantial, separate effort that deserves its own dedicated
> plan-implement-review cycle (matching how Phase 2 itself was deliberately scoped small first),
> not something to bundle into this research-correction pass. See "Corrected conclusion" below.

## Why this was even considered

The original [design spec](../specs/2026-07-12-bunny-magic-containers-design.md) scoped exactly
four phases (HTTP client → single-target deploy → fleet rollout → volume guardrails), all of which
are now shipped (roadmap 2.9 is marked fully shipped). Every one of those phases assumes a Bunny
app and its container **already exist** — created once, out of band, via Bunny's own dashboard
(design spec D5: "each tenant app needs its own env... set once at provision time and left alone").
"Provisioning" — actually creating a new app via API, so a fleet onboarding step could be fully
scripted end-to-end without a human clicking through the dashboard first — is the one real gap left
in that scope. This research pass tried to close it.

## What was tried, and what was actually found

This project's own established research discipline (see `lib/bunny.rhai`'s header comment and the
Phase 2 plan) is: verify against a real, public, checkable source — a GitHub Action's actual source
code, an OpenAPI spec, published code — never guess a request/response shape and ship it. Every
prior phase's shipped behavior traces to one such source
(`BunnyWay/actions/container-update-image/src/action.ts`, fetched successfully despite `docs.bunny.net`
itself being consistently unreachable — every doc/support page hit during this AND prior research
passes returned HTTP 403).

Several angles were tried for an equivalent source for **app creation/deletion**:

1. **The live OpenAPI/Swagger spec.** Bunny's own documentation build config (`docs.json` in the
   public `BunnyWay/documentation` GitHub repo) names the exact source: the Magic Containers API
   reference is generated from `https://api-mc.opsbunny.net/docs/public/swagger.json`. **Fetching it
   returned HTTP 403**, same as every `docs.bunny.net` page in this and prior research passes.
2. **`docs.bunny.net`'s own pages** (API reference overview, "How to deploy and undeploy your app"
   support article). **All 403.**
3. **A third-party community SDK, `ToshY/BunnyNet-PHP`** — its **rendered GitHub Pages doc site**
   (`toshy.github.io/BunnyNet-PHP/magic-containers-api/`) **also 403'd**. This is where the first
   pass of this research stopped and wrongly concluded nothing was findable. **The mistake: the
   same project's actual SOURCE REPOSITORY on github.com was never checked** — `raw.githubusercontent.com`
   URLs are not gated the way `docs.bunny.net`/GitHub-Pages-rendered doc sites are, and this project
   is a generated PHP API client, meaning its source directly encodes the real request shapes.
   **Fetching it worked, and it's a real, verifiable source:**
   - `src/Enum/Endpoint.php`: `MAGIC_CONTAINERS = 'api.bunny.net/mc'` — the identical base URL
     `lib/bunny.rhai` already uses for its verified GET/PATCH.
   - `src/Model/Api/MagicContainers/Applications/AddApplication.php`: `getMethod()` → `POST`,
     `getPath()` → `'apps'`. So: **`POST https://api.bunny.net/mc/apps`** creates an app.
   - `src/Model/Api/MagicContainers/Applications/DeleteApplication.php`: `getMethod()` → `DELETE`,
     `getPath()` → `'apps/%s'` (the app id as a path parameter). So: **`DELETE
     https://api.bunny.net/mc/apps/{appId}`** deletes one.
   - The same directory also has `PatchApplication`/`UpdateApplication` models — full app CRUD, not
     just create/delete.
   - `AddApplication`'s request body (top-level): `name` (string, required), `runtimeType` (string,
     required), `autoScaling` (object, required — `min`/`max`, both int, both required),
     `regionSettings` (object, required — `allowedRegionIds`/`requiredRegionIds` array of string,
     `maxAllowedRegions` int, `nodeSelectors` object), `terminationGracePeriodSeconds` (int,
     optional), `repositorySettings` (object, optional — `templateRepository`/`repositoryName`/
     `owner`, all string), `containerTemplates` (array, optional — image/entryPoint/probes/
     environmentVariables/endpoints/volumeMounts per item), `volumes` (array, optional —
     `name`/`size`).
4. **`BunnyWay/actions`** (the GitHub Action repo Phase 2 successfully used) — checked whether a
   `container-create`-style action exists alongside `container-update-image`. **It does not.** The
   repo has exactly two actions: `deploy-script` and `container-update-image` — neither creates an
   app. (An earlier draft of this document over-read this as evidence Bunny doesn't intend app
   creation to be automatable at all — that inference doesn't hold now that a real client SDK
   demonstrates a working, documented create/delete API; the absence of a first-party GitHub Action
   wrapper just means Bunny didn't publish one, not that the underlying API isn't meant for
   automation.)
5. **A community example project** (`dashpilot/bunny-magic-containers`) — doesn't call a
   create/delete endpoint either, but this is just because that particular project's own scope
   (CI image updates) doesn't need to; not evidence either way about whether creation is
   automatable.

## Provenance caveat — this is a community SDK, not Bunny's own first-party source

Unlike Phase 2's ground truth (`BunnyWay/actions/container-update-image`, which is Bunny's own
official tooling), `ToshY/BunnyNet-PHP` is a third-party, community-maintained client. It IS the
exact source Bunny's own documentation build config points its OWN doc-generation at for the
Magic Containers API reference section (see point 1 above and point 3's doc-site link) — meaning
Bunny's documentation infrastructure treats it as authoritative enough to render docs from — but
it is still one step removed from Bunny's own source, the same way `container_image_tag`'s
`imageTag` field name in `lib/bunny.rhai` is flagged as an inference rather than a first-party
confirmation. Any implementation built on this should carry the same kind of flag, not treat it as
equivalent-confidence to Phase 1-4's ground truth.

## Corrected conclusion

Ground truth for app creation (`POST /mc/apps`) and deletion (`DELETE /mc/apps/{appId}`) **does**
exist, with a real request body schema, sourced from a community SDK that Bunny's own docs
infrastructure treats as authoritative. This is a categorically different situation from Phase 4's
scale/region question (where literally nothing — no source of any kind, first-party or
community — could be found).

**This document does not itself implement Phase 5.** The request body schema above is large
(nested autoscaling, region settings, container templates with probes/endpoints/volume mounts,
top-level volumes) and deserves the same deliberate, incrementally-scoped treatment every prior
phase got — Phase 2 itself was explicitly kept to "single-target image upgrade only," deferring
fleet rollout and guardrails to their own dedicated phases. A responsible first slice of real
Phase 5 would cover only the REQUIRED top-level fields (`name`, `runtimeType`, `autoScaling`,
`regionSettings`) plus `delete_app`, deferring `containerTemplates`/`volumes`/`repositorySettings`
(all optional at creation, per the schema above) to a later slice — exactly the kind of minimal,
non-speculative first cut this codebase's own philosophy already favors. That implementation is
tracked as separate, follow-on work, not bundled into this correction.
