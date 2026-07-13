# Bunny Magic Containers — Phase 5: Provisioning — Research Findings (not implemented)

> This is NOT an implementation plan. It documents a research pass into whether a "Phase 5:
> provisioning" (creating/deleting a Bunny Magic Containers app via API, as opposed to Phases 1-4's
> image-upgrade-only scope) can be built on verified ground truth, the same way every prior phase
> was. Conclusion: **no** — and per this project's own established discipline (see Phase 2's and
> Phase 4's own header comments), that means this phase is not implemented.

## Why this was even considered

The original [design spec](../specs/2026-07-12-bunny-magic-containers-design.md) scoped exactly
four phases (HTTP client → single-target deploy → fleet rollout → volume guardrails), all of which
are now shipped (roadmap 2.9 is marked fully shipped). Every one of those phases assumes a Bunny
app and its container **already exist** — created once, out of band, via Bunny's own dashboard
(design spec D5: "each tenant app needs its own env... set once at provision time and left alone").
"Provisioning" — actually creating a new app via API, so a fleet onboarding step could be fully
scripted end-to-end without a human clicking through the dashboard first — is the one real gap left
in that scope. This research pass tried to close it.

## What was tried

This project's own established research discipline (see `lib/bunny.rhai`'s header comment and the
Phase 2 plan) is: verify against a real, public, checkable source — a GitHub Action's actual source
code, an OpenAPI spec, published code — never guess a request/response shape and ship it. Every
prior phase's shipped behavior traces to one such source
(`BunnyWay/actions/container-update-image/src/action.ts`, fetched successfully despite `docs.bunny.net`
itself being consistently unreachable — every doc/support page hit during this AND prior research
passes returned HTTP 403).

This pass tried five more angles looking for an equivalent source for **app creation**:

1. **The live OpenAPI/Swagger spec.** Bunny's own documentation build config (`docs.json` in the
   public `BunnyWay/documentation` GitHub repo) names the exact source: the Magic Containers API
   reference is generated from `https://api-mc.opsbunny.net/docs/public/swagger.json` — the API's
   own self-documentation, which would have been the single best possible source, better than a
   GitHub Action's source (which only needs to describe the ONE endpoint it calls). **Fetching it
   returned HTTP 403**, same as every `docs.bunny.net` page in this and prior research passes.
2. **`docs.bunny.net`'s own pages** (API reference overview, "How to deploy and undeploy your app"
   support article — the one article whose TITLE suggests it covers exactly this). **All 403.**
3. **A third-party community API reference** (`toshy.github.io/BunnyNet-PHP/magic-containers-api/`,
   a GitHub Pages site, not `docs.bunny.net` itself). **Also 403** — GitHub Pages mirrors of the
   same content hit the same wall.
4. **`BunnyWay/actions`** (the GitHub Action repo Phase 2 successfully used) — checked whether a
   `container-create`-style action exists alongside `container-update-image`. **It does not.**
   The repo has exactly two actions: `deploy-script` and `container-update-image` — neither creates
   an app. This is itself informative: Bunny's own first-party CI tooling has never needed to
   create an app, only update one that already exists.
5. **A community example project** (`dashpilot/bunny-magic-containers`) that uses the real API in
   its own deploy scripts. **No app-creation call found** — it also only drives
   `container-update-image`, consistent with (4).

A general web search did surface one line describing the API's SCOPE in the abstract ("You can
create, modify or delete your Magic Containers applications configuration through the API") — but
with no fetchable source ever yielding the actual endpoint path, method, or request body, that
sentence is not something this project's discipline treats as ground truth. It's exactly the kind
of unconfirmed claim `lib/bunny.rhai`'s header comment already flags one instance of
(`containerTemplates[i].imageTag`) and refuses to build further speculation on top of.

## Conclusion: not implemented, and why that's the correct call

Every phase of this feature so far has been built on a real, checkable source, and has been
explicit whenever something couldn't be independently confirmed (Phase 2's flagged `imageTag`
inference; Phase 4's deliberate refusal to invent a scale/region operation rather than guess at an
unverified API surface). Inventing a `bunny::create_app`/`bunny::provision_app` function against a
completely unverified request/response shape would be the single worst violation of that discipline
yet: unlike a GET whose worst failure mode is a confusing error, a malformed POST to create a
resource risks silently doing SOMETHING against a real Bunny account (a stray app, a
misconfigured one, or a rejected request whose exact failure a caller can't distinguish from
success without a shape to check against) — with no way to verify correctness before a real user
hits it in production.

There is also a real, once-more-look observation from point (4) above worth weighting: Bunny's OWN
first-party automation tooling has never shipped an app-creation Action. Combined with the design
spec's own D5 reasoning (provisioning-time config is set once, by a human, and deliberately left
alone afterward), this suggests app creation may not be a workflow Bunny intends to be automatable
via CI at all — not just a documentation gap this research pass happened to hit.

**This phase is therefore not implemented.** No code changes accompany this document. If a future
session has access to a real Bunny account (to make one real, observed API call and record its
actual shape) or a maintainer obtains real API documentation through an authenticated support
channel, that would unblock this — but guessing is not an acceptable substitute, and roadmap 2.9
remains complete as shipped (all four original phases) without it.
