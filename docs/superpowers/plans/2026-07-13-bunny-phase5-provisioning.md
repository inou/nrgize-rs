# Bunny Magic Containers — Phase 5: Provisioning — Research Findings

> **Two corrections so far, both from independent review rounds:**
> 1. **(Opus round)** An earlier version of this document concluded ground truth for app
>    creation/deletion could not be found anywhere. That was **wrong** — it only checked a
>    third-party SDK's rendered doc site, not that same project's actual source code, which is a
>    real, checkable source (below).
> 2. **(Fable round)** The first correction then overclaimed the SDK's authority — asserting
>    Bunny's own documentation build config points at it. **That's also wrong** (see "Provenance"
>    below) — it does not, and this document conflated two unrelated findings. The 403s hit
>    throughout this research were also mis-attributed to Bunny's servers; they're actually this
>    session's own outbound network policy (see "Why the 403s happened" below), which changes what
>    "not found" even means here.
>
> Corrected conclusion (see bottom): ground truth for the API shape **does** exist via a
> community SDK, one step removed from Bunny's own first-party source. Implementing Phase 5 is
> still deferred — not because nothing is verifiable, but because (a) the request schema is large
> enough to deserve its own dedicated slice, matching how Phase 2 itself was scoped small first,
> and (b) a session without this one's network restriction should try the first-party swagger spec
> directly before building on the secondary source.

## Why this was even considered

The original [design spec](../specs/2026-07-12-bunny-magic-containers-design.md) scoped exactly
four phases (HTTP client → single-target deploy → fleet rollout → volume guardrails), all of which
are now shipped (roadmap 2.9 is marked fully shipped). Every one of those phases assumes a Bunny
app and its container **already exist** — created once, out of band, via Bunny's own dashboard
(design spec D5: "each tenant app needs its own env... set once at provision time and left alone").
"Provisioning" — actually creating a new app via API, so a fleet onboarding step could be fully
scripted end-to-end without a human clicking through the dashboard first — is the one real gap left
in that scope. This research pass tried to close it.

## Why the 403s happened (this session's network policy, not Bunny's servers)

Every attempt in this research to reach `docs.bunny.net` or the Magic Containers API's own
subdomain returned HTTP 403. This document's earlier drafts read that as "Bunny's servers are
unreachable" — checking this session's own outbound-proxy status
(`curl "$HTTPS_PROXY/__agentproxy/status"`) shows the real cause:

```
"recentRelayFailures": [
  {"kind": "connect_rejected", "detail": "gateway answered 403 to CONNECT (policy denial or upstream failure)", "host": "api-mc.opsbunny.net:443"},
  {"kind": "connect_rejected", "detail": "gateway answered 403 to CONNECT (policy denial or upstream failure)", "host": "docs.bunny.net:443"}
]
```

`/root/.ccr/README.md`'s own troubleshooting section is explicit: "403/407 from the proxy: The
destination host is not allowed by your organization's egress policy for this session. Do not
retry or route around it." `raw.githubusercontent.com` happens to be reachable from this session;
`bunny.net` domains are not — that's a fact about THIS session's egress allowlist, not about
whether Bunny's own documentation site or its live OpenAPI spec are otherwise publicly reachable.
**A session or environment without this restriction could very plausibly fetch
`api-mc.opsbunny.net/docs/public/swagger.json` directly** — first-party ground truth, at
Phase-1-4 confidence, rather than the community-SDK-derived finding below. That should be the
first thing a future implementation attempt tries, before building on the secondary source.

## What was tried, and what was actually found

This project's own established research discipline (see `lib/bunny.rhai`'s header comment and the
Phase 2 plan) is: verify against a real, public, checkable source — a GitHub Action's actual source
code, an OpenAPI spec, published code — never guess a request/response shape and ship it. Every
prior phase's shipped behavior traces to one such source
(`BunnyWay/actions/container-update-image/src/action.ts`).

Several angles were tried for an equivalent source for **app creation/deletion**:

1. **The live OpenAPI/Swagger spec.** Bunny's own documentation build config (`docs.json` in the
   public `BunnyWay/documentation` GitHub repo) names the exact source: the Magic Containers API
   reference is generated from `https://api-mc.opsbunny.net/docs/public/swagger.json`. **This
   session's proxy blocked the CONNECT** (see above) — not evidence the spec itself is
   unreachable in general.
2. **`docs.bunny.net`'s own pages** (API reference overview, "How to deploy and undeploy your app"
   support article). **Same proxy block**, same caveat.
3. **A third-party community SDK, `ToshY/BunnyNet-PHP`.** Its **rendered GitHub Pages doc site**
   (`toshy.github.io/BunnyNet-PHP/magic-containers-api/`) hit the same proxy block. **This is
   where the first research pass stopped and wrongly concluded nothing was findable** — the
   mistake was never checking the same project's actual SOURCE REPOSITORY on github.com, which
   IS reachable from this session (`raw.githubusercontent.com` isn't in the blocked set) and, as
   a generated PHP API client, directly encodes the real request shapes. **Fetching it worked —
   independently re-verified twice, once per review round, by fetching the raw files directly:**
   - `src/Enum/Endpoint.php`: `MAGIC_CONTAINERS = 'api.bunny.net/mc'` — the identical base URL
     `lib/bunny.rhai` already uses for its verified GET/PATCH.
   - `src/Model/Api/MagicContainers/Applications/AddApplication.php`: `getMethod()` → `POST`,
     `getPath()` → `'apps'`. So: **`POST https://api.bunny.net/mc/apps`** creates an app.
   - `src/Model/Api/MagicContainers/Applications/DeleteApplication.php`: `getMethod()` → `DELETE`,
     `getPath()` → `'apps/%s'` (the app id as a path parameter), and it does **not** implement a
     body-model interface — deletion takes no request body. So: **`DELETE
     https://api.bunny.net/mc/apps/{appId}`** deletes one, with no body.
   - The same directory also has `PatchApplication`/`UpdateApplication`/`GetApplication`/
     `ListApplications` models — full app CRUD, not just create/delete.
   - `AddApplication`'s request body (top-level): `name` (string, required), `runtimeType` (string,
     required), `autoScaling` (object, required — `min`/`max`, both int, both required),
     `regionSettings` (object, required — `allowedRegionIds`/`requiredRegionIds` array of string,
     `maxAllowedRegions` int, `nodeSelectors` object), `terminationGracePeriodSeconds` (int,
     optional), `repositorySettings` (object, optional — `templateRepository`/`repositoryName`/
     `owner`, all string), `containerTemplates` (array, optional — each item itself requires
     `name`/`imageName`/`imageNamespace`/`imageTag`/`imageRegistryId`, plus optional
     entryPoint/probes/environmentVariables/endpoints/volumeMounts), `volumes` (array, optional —
     `name`/`size`).
     - Note: `containerTemplates`'s per-item required `imageTag` field is independent
       corroboration of the SAME field name `lib/bunny.rhai`'s header comment already flags as an
       inference on the GET-response side (`container_image_tag`) — a small additional data point
       in favor of that inference being correct, not full confirmation of it.
4. **`BunnyWay/actions`** (the GitHub Action repo Phase 2 successfully used) — checked whether a
   `container-create`-style action exists alongside `container-update-image`. **It does not.** The
   repo has exactly two actions: `deploy-script` and `container-update-image` — neither creates an
   app. (An earlier draft over-read this as evidence Bunny doesn't intend app creation to be
   automatable at all — that inference doesn't hold now that a real client SDK demonstrates a
   working, documented create/delete API; the absence of a first-party GitHub Action wrapper just
   means Bunny didn't publish one, not that the underlying API isn't meant for automation.)
5. **A community example project** (`dashpilot/bunny-magic-containers`) — doesn't call a
   create/delete endpoint either, but this is just because that particular project's own scope
   (CI image updates) doesn't need to; not evidence either way about whether creation is
   automatable.

## Provenance caveat — this is a community SDK, not Bunny's own first-party source

Unlike Phase 2's ground truth (`BunnyWay/actions/container-update-image`, which is Bunny's own
official tooling), `ToshY/BunnyNet-PHP` is a third-party, community-maintained client, with no
established endorsement relationship from Bunny — **a prior version of this document incorrectly
claimed Bunny's own documentation build config points at it; it does not** (`docs.json` points at
the `api-mc.opsbunny.net` swagger spec — a completely different, first-party source — and never
references this SDK at all; that was this document's own conflation of two unrelated findings,
caught and removed here). The SDK is simply a generated client whose source code happens to be
reachable from this session when the doc sites weren't. Any implementation built on it should
carry the same kind of flag `lib/bunny.rhai` already uses for its one inferred field name
(`container_image_tag`) — a reasonable working assumption, not a first-party confirmation — and
should ideally be re-verified against the first-party swagger spec once that's fetchable (see
"Why the 403s happened" above).

## Corrected conclusion

Ground truth for app creation (`POST /mc/apps`) and deletion (`DELETE /mc/apps/{appId}`, no body)
**does** exist, with a real request body schema, sourced from a community SDK. This is a
categorically different situation from Phase 4's scale/region question (where literally nothing —
no source of any kind, first-party or community — could be found), but a step below Phases 1-4's
first-party-sourced confidence.

**This document does not itself implement Phase 5.** Two independent reasons: (a) the request body
schema is large (nested autoscaling, region settings, container templates with per-item required
image fields plus probes/endpoints/volume mounts, top-level volumes) and deserves the same
deliberate, incrementally-scoped treatment every prior phase got — Phase 2 itself was explicitly
kept to "single-target image upgrade only," deferring fleet rollout and guardrails to their own
dedicated phases; (b) this session's own network restriction blocked the first-party swagger spec
that could give stronger ground truth than the community SDK — a future attempt should try that
first, from an unrestricted environment, before building on the secondary source here.

A responsible first slice of real Phase 5, once either source is in hand, would cover only the
REQUIRED top-level fields (`name`, `runtimeType`, `autoScaling`, `regionSettings`) plus
`delete_app`, deferring `containerTemplates`/`volumes`/`repositorySettings` to a later slice —
though note `containerTemplates` is optional only at the schema level; whether an app with zero
containers is actually usable through this module's existing PATCH-an-existing-container verb (or
requires at least one container template at creation time) is unverified and should be checked
before assuming a container-less first slice is viable. That implementation is tracked as
separate, follow-on work, not bundled into this research pass.
