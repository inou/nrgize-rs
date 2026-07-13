# Bunny Magic Containers — Phase 1: A real HTTP client (headers + verbs) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`)
> syntax. Read `docs/superpowers/specs/2026-07-12-bunny-magic-containers-design.md` first — this
> plan implements exactly D1/D2 from that spec, nothing more. Phases 2–4 (the actual Bunny provider
> module, fleet-scale rollout, volume guardrails) are follow-on plans, not part of this one.

**Goal:** `http_get`/`http_post` today cannot send custom headers (no `Authorization`), and there is
no `http_put`/`http_patch`/`http_delete` — insufficient to drive any real authenticated REST API.
Add header support to the existing verbs and add the three missing verbs, preserving every existing
call site and test byte-for-byte (this is a pure capability addition, additive-only overloads).

**Architecture:** `src/engine/builtins/http.rs` currently has `agent(timeout_secs)` (builds a
`ureq::Agent`), `do_get`/`do_post` (the actual request execution), and `register()` (wires
`http_get`/`sim_http_healthy`/`http_post` into the Rhai `Engine`, with dry-run short-circuiting: GET
is an honest live read even under `--dry-run`, POST is a write and short-circuits to a synthetic
`200` + a recorded `check` action). This plan:
1. Generalizes `do_get`/`do_post` into one `do_request(method, url, body: Option<&str>, headers:
   &[(String, String)], timeout_secs)` helper (or an equivalent shape — the exact signature is an
   implementation choice, the constraint is ONE shared request path, not five near-duplicates).
2. Adds a `headers_from_dynamic(Dynamic) -> Result<Vec<(String,String)>, Box<EvalAltResult>>`
   conversion for a Rhai map argument (`#{"Authorization": "Bearer " + token}`), throwing a clear
   Rhai-catchable error on a non-map / non-string-valued argument rather than panicking.
3. Registers header-accepting **overloads** of every existing verb, plus brand-new `http_put`,
   `http_patch`, `http_delete` (each with a headers overload too) — mirroring the existing
   `sim_http_healthy(url)` / `sim_http_healthy(url, timeout_secs)` two-overload pattern already in
   this file, so this is a proven idiom in this codebase, not a new one.
4. Applies the SAME dry-run semantics already established: GET (with or without headers) is a real,
   honest live read; POST/PUT/PATCH/DELETE (with or without headers) are writes and short-circuit
   under `--dry-run` to a synthetic `200` + a recorded `check` action, exactly like `http_post` does
   today.
5. Redacts secret-valued headers from anything written to the dry-run plan log or `print`/`debug`
   output — **read `src/engine/secret.rs` and its existing `redact()` call sites (there is at least
   one in the SSH stdin/env-file delivery path per `docs/architecture.md`'s description of
   `secret.rs`) before implementing this**, and reuse that mechanism rather than inventing a second
   redaction path. An `Authorization` header value is exactly the kind of thing that must never
   appear in plaintext in a dry-run plan render or an error message.

**Tech Stack:** `ureq 3` (already a dependency, already used via `agent()`), `rhai` (`Dynamic`/`Map`
for the headers argument), the existing `Secret` type in `src/engine/secret.rs`.

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/builtins/http.rs` | The whole change: shared request helper, header conversion, new verb registrations, dry-run + redaction semantics, tests. |
| `docs/builtins.md` | Living reference (per `CONTRIBUTING.md`: "Keep `docs/*.md` in sync with behavior changes") — add `http_put`/`http_patch`/`http_delete` sections and document the headers-argument overloads on every verb, mirroring the existing `http_get`/`http_post` doc format exactly. |
| `docs/roadmap.md` | Flip the new "2.9 PaaS provider targets" item's Phase 1 line from open to shipped once this lands (see the roadmap entry this plan fulfills). |

**Deliberate deferral (documented, not silent):** this plan does **not** touch `CommandRunner`,
does **not** add any Bunny-specific builtin, and does **not** add response header access (only
request headers) — `HttpResponse` stays `{status, body}`. A future phase can widen `HttpResponse`
if a real Bunny response header ever needs reading; nothing in Phase 2's design requires it today
(Bunny's API is expected to communicate state via JSON body + status code, not response headers).

---

## Task 1: Read the redaction mechanism before touching anything

**Files:** Read-only — `src/engine/secret.rs`, and grep the codebase for its call sites.

- [ ] **Step 1: Understand the existing `Secret` type and `redact()`**

Read `src/engine/secret.rs` in full. Answer, in a comment at the top of your working notes (not
committed): how is a Rhai-level secret represented (a wrapped `Dynamic` type, a string convention,
something else)? How does `redact()` find secret substrings in an arbitrary string? Is there an
existing call site where a *header-shaped* value (a `key: value` pair) is redacted, or only whole
command lines / env values so far?

- [ ] **Step 2: Decide the header-secret story**

Based on Step 1, decide: can a script pass a `Secret`-wrapped value as a header value directly
(e.g. `#{"Authorization": "Bearer " + secret("bunny_token")}`), and does string concatenation with
a `Secret` already redact correctly elsewhere in this codebase? If concatenation already produces a
redactable string (check `docs/safety.md`'s secrets section), the header path may need **no new
code** here beyond routing the final header string through the same `redact()` call the plan-log
writer already uses for other recorded actions. Write down the concrete plan for Task 4 before
proceeding — this step exists specifically so Task 4 isn't guessed at.

---

## Task 2: Generalize the request execution path

**Files:** Modify `src/engine/builtins/http.rs`

- [ ] **Step 1: Write the failing tests**

Add tests (alongside the existing ones in this file's `#[cfg(test)] mod tests`) using the existing
`spawn_http_responder`-style real-listener pattern already in this file (do not mock — this
codebase's own convention, per the existing GET/POST tests, is to exercise the real `ureq` round
trip):

- A GET with a custom header actually sends it — spin up a listener that reads the request,
  asserts the raw bytes contain `Authorization: Bearer test-token`, and returns 200.
- A PUT sends its body and a custom header, and the response status/body round-trip correctly
  (mirror `http_post_sends_its_body_and_extracts_a_real_response`, one for `http_put`).
- A PATCH does the same.
- A DELETE (no body) sends its header and reads the response.
- `http_get`/`http_put`/`http_patch`/`http_delete` all correctly surface a real non-2xx status +
  body (mirror `http_get_extracts_status_and_body_on_a_real_5xx_response_instead_of_a_transport_error`)
  — this must keep working identically for the new verbs, since a Bunny API returning a 404/409 with
  a JSON error body is exactly the case a provider script needs to inspect, not have collapsed to a
  transport error.
- Every write verb (POST already covered; add PUT/PATCH/DELETE) short-circuits under `--dry-run` to
  a synthetic `200` and records a `check` action — mirror the existing dry-run test structure.
- GET with headers is still a real, honest live read under `--dry-run` (mirror
  `http_get_probes_for_real_in_dry_run`), not short-circuited — headers must not change the
  live-vs-simulated classification of a verb.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `CI=true cargo test --all-targets --locked http`
Expected: compile failure (the new fns don't exist yet) or clear test failures once stubs compile.

- [ ] **Step 3: Implement the shared request helper**

Refactor `do_get`/`do_post` into one internal helper covering GET/POST/PUT/PATCH/DELETE with an
optional body and a header list, preserving the existing `http_status_as_error(false)` semantics
(a non-2xx is a real `Ok` with status+body intact — this is load-bearing, see the comment already
in `agent()`) and the existing `status: 0` = "transport failure, not an HTTP response" convention.
Keep `do_get`/`do_post`'s existing external behavior identical — this is an internal refactor, not
a behavior change, for the two verbs that already exist.

- [ ] **Step 4: Implement `headers_from_dynamic`**

Convert a Rhai map argument into an ordered `Vec<(String, String)>` (or whatever the request helper
from Step 3 expects), applying the redaction decision from Task 1 Step 2. Reject (via a Rhai
`EvalAltResult`, not a panic) a non-map argument or a map value that isn't string-representable,
with an error message that names the offending key so a script author can fix it without guessing.

- [ ] **Step 5: Register the new/overloaded builtins**

`http_get(url, headers)`, `http_post(url, body, headers)`, `http_put(url, body)`,
`http_put(url, body, headers)`, `http_patch(url, body)`, `http_patch(url, body, headers)`,
`http_delete(url)`, `http_delete(url, headers)` — mirroring the existing multi-overload
`sim_http_healthy` registration pattern in this same file exactly (same style of repeated
`engine.register_fn("name", closure)` calls with different arities).

- [ ] **Step 6: Run the tests**

Run: `CI=true cargo test --all-targets --locked http`
Expected: all pass, including every pre-existing test in this file unchanged.

- [ ] **Step 7: Mutation-verify the dry-run gate**

Per `CONTRIBUTING.md`'s mutation-testing discipline: temporarily invert the `EffectMode::DryRun`
check for one of the new write verbs, confirm its dry-run test now fails (a real request would go
out), then restore it and confirm the suite passes again. This catches a test that would pass even
if the dry-run gate were silently removed.

- [ ] **Step 8: Commit**

```bash
git add src/engine/builtins/http.rs
git commit -m "feat(http): headers + PUT/PATCH/DELETE builtins for real REST APIs"
```

---

## Task 3: `cargo clippy` + the full local gate

**Files:** None (verification only)

- [ ] **Step 1: Run the full gate from `CONTRIBUTING.md`**

```bash
cargo build --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
CI=true cargo test --all-targets --locked
```

All three must pass clean before proceeding — this is the project's own stated bar for every PR.

---

## Task 4: Update the living docs

**Files:** Modify `docs/builtins.md`, `docs/roadmap.md`

- [ ] **Step 1: Document the new verbs**

Add `### http_put(url, body) -> HttpResponse`, `### http_patch(...)`, `### http_delete(...)`
sections to `docs/builtins.md`'s `## HTTP` block, matching the existing `http_get`/`http_post`
section format exactly (Live/DryRun bullet pairs, a short `rhai` example). Document the headers
overload on every verb in the same section rather than as separate entries — a `headers?` optional
final argument, with one example showing `#{"Authorization": "Bearer " + token}`.

- [ ] **Step 2: Add the roadmap entry**

In `docs/roadmap.md`'s Tier 2 section, add (if not already present from the design-spec companion
edit):

```markdown
### 2.9 PaaS provider targets (Bunny Magic Containers, similar platforms) — **L** — Phase 1 ✅ shipped, Phases 2-4 open

`nrg` today only deploys to SSH-reachable Docker hosts. A managed container PaaS (Bunny Magic
Containers, and by extension anything API-driven rather than SSH-driven) needs a second deploy
target. See [the design spec](superpowers/specs/2026-07-12-bunny-magic-containers-design.md) for
the full phase breakdown. Phase 1 (this item) is a prerequisite for all of it: the HTTP builtins
gained header support and PUT/PATCH/DELETE, enough to drive a real authenticated REST API from Rhai
stdlib alone — no new Rust needed for the provider module itself (Phase 2).
```

- [ ] **Step 2: Commit**

```bash
git add docs/builtins.md docs/roadmap.md
git commit -m "docs: document http_put/patch/delete + headers, track Bunny provider roadmap item"
```

---

## Definition of done

- [ ] Every existing test in `src/engine/builtins/http.rs` still passes, unmodified in behavior.
- [ ] New tests cover: headers actually reach the wire (all five verbs), a real non-2xx status+body
  round-trips for every verb, every write verb short-circuits under dry-run, GET stays a real read
  under dry-run regardless of headers, and a secret-valued header is never written to a plan-log
  line or error message in plaintext.
- [ ] `cargo build --all-targets --locked && cargo clippy --all-targets --locked -- -D warnings &&
  CI=true cargo test --all-targets --locked` all pass clean.
- [ ] `docs/builtins.md` and `docs/roadmap.md` reflect the new capability.
- [ ] A follow-up session can start Phase 2 (`lib/bunny.rhai`) using only Rhai stdlib + these new
  builtins — no further Rust engine changes required for a basic single-app image upgrade.
