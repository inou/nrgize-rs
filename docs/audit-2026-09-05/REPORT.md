# nrg deployment, robustness, and security audit

Date: 2026-09-05. Audited revision: `a2009367402b58d3c4b4051a7e8b5849d5732972`, version `0.1.1`. The working tree was clean at the start. This audit adds documentation and reproduction artifacts; it does not change the implementation.

**Assessment: production hardening is needed.** The Rust core has useful safeguards, but the deployment state machine does not consistently preserve them across partial failures. I would resolve A01–A09 before relying on the advertised zero-downtime and secret-protection guarantees. The Caddy configuration also serves matching application routes over plaintext HTTP.

There are **24 findings and hardening gaps: 5 High, 16 Medium, 3 Low**. Severity reflects potential impact under the stated conditions; it does not imply equal likelihood. No unauthenticated remote code-execution path was confirmed. The absence of such a finding is not a security certification.

## Scope and evidence

Reviewed the Rust CLI and engine, embedded-module resolver, state and locking, secret discovery and age integration, SSH subprocess handling, HTTP builtins, Rhai deployment/runtime/registry/proxy/health/accessory/Bunny modules, examples, installation/release workflows, tests, and the relevant operational documentation. Previous reviews were treated as historical context and checked against current source.

Validation included the compiled CLI, local filesystem probes, HTTP servers bound to loopback, real age 1.3.2, and real Caddy 2.10.0. SSH deployment reproductions use a command-recording fake SSH executable: they exercise the real nrg/Rhai control flow and inject remote results, without connecting to a fleet. They prove the resulting decisions and persisted state; they do not reproduce a real distributed network partition. The Caddy check preserves the listener/route shape but substitutes local ports and storage and disables certificate acquisition to avoid contacting an ACME service.

No production deployment, registry push, database operation, live Bunny API call, or real credential inspection was performed. No fuzzing campaign, penetration test against deployed infrastructure, exhaustive model checking, or deployment-host OS audit was performed. Docker, Podman, nerdctl, and Apple Container runtime behavior was reviewed through the command construction and tests rather than live fleet installations. Cloud firewall policy, SSH server policy, real secret-manager adapters, and GitHub branch/tag protections remain outside verified scope.

### Checks run

| Check | Result |
|---|---|
| `cargo test --all-targets --locked --no-fail-fast`, with real age on PATH | **731 passed, 1 failed**, across 39 test targets, on macOS arm64 |
| Failing test | `tests/secrets.rs::secret_cmd_framing_fetches_via_a_local_command`: `echo -n` produced the literal `-n` prefix on this system; use `printf` |
| `cargo clippy --all-targets --locked -- -D warnings` | Passed; all targets compiled |
| `cargo fmt --all -- --check` | Failed; formatting differences throughout the source tree |
| cargo-audit 0.22.2 | **0 known vulnerabilities**, 1 unmaintained dependency warning |
| Local CLI/filesystem/HTTP fault probes | 15 defect assertions reproduced |
| Supplemental real age/Caddy checks | 3 defect assertions reproduced |

The first sandboxed test run failed at loopback socket creation; rerunning with local networking permitted resolved those failures. An initial installer test server startup timeout did not recur in the final full run. Initially age was absent and its tests self-skipped while reporting passes; the final run used real age, so those results are not merely skips. Some platform/privilege-specific tests still self-skip by design, particularly Linux-only coverage on this macOS host.

The advisory database contained 1,239 advisories, at commit `5a0ebedfe8bdd2e295b171f4162f8c977bcad9a5`, last updated 2026-09-02. The warning is `RUSTSEC-2026-0249`, `smartstring 1.0.1`, reached through `rhai 1.25.1`. It is an unmaintained-package notice, not a discovered vulnerability. [RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0249.html).

## Findings at a glance

| ID | Severity | Finding | Evidence |
|---|---|---|---|
| A01 | High | Lost proxy-switch acknowledgment can cause removal of the serving container | Fault injection |
| A02 | High | Cleanup failure preserves a retired target while declaring the new image deployed | Fault injection |
| A03 | High | Failed deploy/rollback overwrites the last rollback image | Fault injection |
| A04 | High | Caddy domain routes remain reachable over plaintext HTTP | Real Caddy |
| A05 | High | Every successful deploy prunes unrelated stopped containers | Source + Docker specification |
| A06 | Medium | Remote env-file writes follow symlinks and preserve unsafe permissions | Filesystem reproduction |
| A07 | Medium | Remote-build archive uses a predictable local temporary filename | Real archive overwrite |
| A08 | Medium | Failed unseal leaves readable partial plaintext | Real age |
| A09 | Medium | Rollback errors and persisted-config replay bypass secret redaction | CLI reproduction |
| A10 | Medium | Cross-origin redirects forward custom API-key headers | Loopback HTTP reproduction |
| A11 | Medium | Large age values deadlock the encryption/decryption subprocess pattern | Real age encryption reproduction |
| A12 | Medium | Commands have no execution deadline or bounded output | Source |
| A13 | Medium | Distributed locks leak on interrupts and do not cover overlapping fleets | Signal injection + source |
| A14 | Medium | Remove bypasses deployment locks and incompletely purges state | CLI reproduction + source |
| A15 | Medium | Native rollback ignores the deployed container runtime | CLI reproduction |
| A16 | Medium | Destination support is missing from day-2 commands | CLI/source |
| A17 | Medium | Dry-run resolves file secrets from the wrong root in subdirectories | CLI reproduction |
| A18 | Medium | HTTP response-body failures become empty successful responses | Loopback HTTP reproduction |
| A19 | Medium | Fresh Caddy setup does not create its required configuration directory | Source |
| A20 | Medium | Trust checks deliberately accept other group members' write access | Source; conditional threat model |
| A21 | Medium | Bunny rollout checks configuration, with incomplete rollback identity | Source; live behavior unverified |
| A22 | Low | Bunny is absent from the embedded/vendor catalog | CLI reproduction |
| A23 | Low | Audit trail can omit operations/errors and mislabel commands | Source |
| A24 | Low | CI/release/distribution hardening remains incomplete | Tool results + workflow review |

## High-priority correctness and security failures

### A01 — Preserve the new container when the forward switch outcome is unknown

Locations: [lib/deploy.rhai:875](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:875), [lib/deploy.rhai:919](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:919), [lib/deploy.rhai:920](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:920).

The removal compensation is guarded by `proxy_switched && !proxy_restored`. However, `proxy_switched` becomes true only **after** `px_deploy` returns successfully. A proxy can apply the new route and then lose its SSH/HTTP acknowledgment. The function throws with `proxy_switched == false`. If restoration also fails, the removal compensation still removes the new container, potentially taking down the backend that the proxy actually uses.

Reproduced by injecting a failed acknowledgment for the forward switch and another failure for restoration. nrg then issued `docker rm -f 'app-web-v3-13000'`. The test models the committed-but-unacknowledged effect; it does not claim to have caused a real SSH partition.

**Remedy:** model the forward switch as attempted/unknown before dispatch. Remove the new container only when the route is verified to point elsewhere or restoration is confirmed. Add reconciliation for ambiguous results. The same uncertainty principle applies to container creation: a failed `docker run` reply does not establish that no container was created, and the current compensation is registered only after reported success.

### A02 — Persist the active route independently of post-commit cleanup

Locations: [lib/deploy.rhai:476](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:476), [lib/deploy.rhai:497](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:497), [lib/deploy.rhai:518](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:518), [lib/deploy.rhai:577](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:577).

After all proxy switches commit, nrg renames containers, stops/removes the old one, and prunes. If **any** of those steps reports failure, `continue` skips persistence of the new host port and target. Global version/image/config are still advanced and the command can exit successfully.

Reproduction: the new route was switched to `localhost:13000`; only the later cleanup call failed. The run exited 0, saved image `repo:v3`, and left target `localhost:13001`. The old container's stop/removal commands had already been issued. A later deploy will trust that obsolete target for compensation and can restore traffic to a retired backend. Failure during rename has additional ambiguity because subsequent steps continue unconditionally.

**Remedy:** keep active traffic state separate from cleanup status. Record each verified switch and enough previous/new container identity to reconcile safely. Use a deploy-level journal or equivalent durable recovery record, and atomically commit related state fields. An atomic write of each individual key does not make the whole deployment crash-atomic. Test interruptions and I/O failures between every cutover/rename/state step.

### A03 — Commit rollback history only after a successful deployment

Locations: [lib/deploy.rhai:401](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:401), [lib/deploy.rhai:1092](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:1092).

Both `deploy()` and `rollback()` overwrite `<service>.prev` before the operation succeeds. Their catch blocks release the remote lock but do not restore the predecessor.

Reproduction: current image `repo:v2`, previous `repo:v1`. A rollback to v1 failed at image pull. State remained on v2, but `.prev` had become v2. Retrying ordinary rollback now targets the very image the operator was trying to escape. A failed deployment of v3 similarly replaces the v1 predecessor with v2 while leaving v2 current.

**Remedy:** snapshot original state in memory/durable journal and commit the predecessor only together with the successful release record. Avoid the growing list of early-validation special cases: any later network, health, disk, or configuration error can cause the same problem. Store a small release history instead of relying on one mutable pointer. If rollback is intended to restore a release's configuration too, retain previous config alongside previous image; the current code replays the most recently persisted config.

### A04 — Caddy serves the domain-matched application on HTTP as well as HTTPS

Locations: [lib/caddy.rhai:69](/Users/inou/dev/rust/nrgize-rs/lib/caddy.rhai:69), [lib/caddy.rhai:140](/Users/inou/dev/rust/nrgize-rs/lib/caddy.rhai:140).

The same Caddy server listens on both `:80` and `:443`, with application routes matching only the host name. Caddy inserts automatic HTTP-to-HTTPS redirects **after user routes with host matchers**, so the application route handles matching HTTP requests first. Configuring `cfg.domain` and automatic certificates therefore does not enforce HTTPS.

A real Caddy 2.10.0 instance using this listener/route shape returned HTTP 200 and the backend's `APPLICATION_PLAINTEXT` response on the HTTP listener. Local high ports replaced 80/443; certificate acquisition was disabled for isolation, while redirect logic remained enabled. This matches Caddy's documented route ordering. [Caddy automatic HTTPS](https://caddyserver.com/docs/automatic-https).

**Impact:** plaintext application access can expose credentials or requests from clients that use HTTP. Secure-cookie behavior depends on the application and browser; this does not prove every cookie leaks. HTTPS still may function normally, making the parallel HTTP exposure easy to miss.

**Remedy:** put application routes on an HTTPS-only server and let Caddy create its HTTP redirect server, or install a correctly ordered explicit redirect before HTTP application routing. Test HTTP status/Location and HTTPS response behavior, not just that the generated command mentions the domain.

### A05 — Default cleanup deletes stopped containers belonging to other workloads

Locations: [lib/docker.rhai:612](/Users/inou/dev/rust/nrgize-rs/lib/docker.rhai:612), [lib/deploy.rhai:480](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:480).

Every successful deployment calls `docker container prune -f` without a service/ownership label filter. Docker defines this as removal of **all stopped containers**. This is host-wide cleanup, even when the user is deploying one service. [Docker container prune](https://docs.docker.com/reference/cli/docker/container/prune/).

A stopped unrelated container's writable layer, logs, and restartable container definition can be lost. This also conflicts with `accessory_stop()`'s promise to retain an accessory for later restart. Named volumes are not deleted by this command; the finding does not claim otherwise. The sibling unscoped image prune also affects host-wide image retention.

**Remedy:** remove only container IDs owned by this deployment, or label managed resources and apply filters. Make broader host cleanup an explicit separate command. Stop suppressing every daemon error with `2>/dev/null || true`; idempotent “already absent” handling should not hide permission errors, daemon failures, or failed cleanup.

## Security boundary failures

### A06 — Remote secret writes are neither symlink-safe nor reliably 0600

Locations: [src/engine/builtins/exec.rs:226](/Users/inou/dev/rust/nrgize-rs/src/engine/builtins/exec.rs:226), [lib/docker.rhai:384](/Users/inou/dev/rust/nrgize-rs/lib/docker.rhai:384), [lib/docker.rhai:438](/Users/inou/dev/rust/nrgize-rs/lib/docker.rhai:438).

`write_remote` executes `umask 077; cat > PATH`. A umask affects newly created files; it does not tighten an existing file's mode, and shell redirection follows symlinks. Both application and release-task env files use predictable `/tmp` paths. They are not removed after use, including after container cleanup.

Reproduction through the actual builtin and a local SSH shim: a symlink was followed, its target overwritten with dummy secret text, and the target remained 0644. A local attacker can exploit predictable paths on a host whose filesystem protections allow it; Linux protected-symlink/regular-file settings may block some variants and must not be assumed universally. Existing overly readable files need no symlink race.

**Remedy:** allocate a private directory and exclusive, unpredictable files, establish 0600 before writing, reject symlinks, and clean up on success/failure. For general config updates, write a fresh private file and publish atomically. Do not patch this solely with a chmod after writing. This core issue remains present from the earlier security review.

### A07 — Local archive creation for a remote build can overwrite another file

Locations: [lib/docker.rhai:117](/Users/inou/dev/rust/nrgize-rs/lib/docker.rhai:117), [lib/docker.rhai:229](/Users/inou/dev/rust/nrgize-rs/lib/docker.rhai:229).

The remote-build archive is created locally as `/tmp/.nrg-build-ctx-<sanitized-tag>.local.tgz`. It is predictable across users, projects, and concurrent builds; the tag sanitizer is not a uniqueness mechanism. `tar -czf` is run against it without exclusive creation. `umask 077` does not protect an existing target.

Reproduced using the actual remote-build path: a precreated symlink redirected the archive write into a fixture file, replacing its contents while preserving mode 0644. This can expose a source archive or corrupt a file writable by the operator, subject to local OS protections. Separate builds sharing a tag can also race over the same archive/remote directory without an attacker.

**Remedy:** create the archive with an OS-created unique private temporary file/directory and retain that identity through transfer. Use a unique remote directory too, rather than delete-and-recreate a shared tag-derived path. The new root credential exclusions help confidentiality of a correctly created archive; they do not fix its temporary-file lifecycle.

### A08 — Failed unseal leaves partial plaintext with umask-derived permissions

Location: [src/secrets/mod.rs:447](/Users/inou/dev/rust/nrgize-rs/src/secrets/mod.rs:447).

Unseal gives age the final output path directly. The owner-only chmod happens only after successful completion. Streaming decryption can authenticate and write earlier chunks before detecting corruption in a later chunk. On failure, nrg returns without tightening permissions or removing the partial output.

With real age 1.3.2, a 200 KB encrypted fixture with its last byte corrupted produced exit 1 and left **196,608 bytes of plaintext at mode 0644** under umask 022. The current malformed-header test fails before any plaintext is emitted, so it misses this case. Successful decryption also has an exposure window before chmod when the umask is permissive.

**Remedy:** decrypt into a fresh 0600 temporary file, remove it on failure, and atomically publish only after age succeeds. Enforce the non-overwrite/force policy at publication, with symlink-safe behavior, instead of an existence check followed by an external write to the final path.

### A09 — Secret redaction is bypassed during compensation and config replay

Locations: [src/engine/transaction.rs:85](/Users/inou/dev/rust/nrgize-rs/src/engine/transaction.rs:85), [lib/deploy.rhai:1023](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:1023), [src/engine/eval.rs:175](/Users/inou/dev/rust/nrgize-rs/src/engine/eval.rs:175), [src/engine/secret.rs:261](/Users/inou/dev/rust/nrgize-rs/src/engine/secret.rs:261).

Two reproduced gaps:

1. Compensation failures are printed directly with `eprintln!(... {ce})`, outside the engine's redaction hooks. A registered dummy secret thrown from an `on_rollback` callback appeared in stderr.
2. Persisted deployment config contains ordinary revealed strings. A fresh `nrg rollback` loads that config without registering its credentials as secrets. A simulated `docker run` error echoing the replayed dummy password was printed unredacted and can enter the audit outcome as well.

Other output boundaries require the same scrutiny: `write_remote` traces its path directly; the command precheck traces host labels directly; arbitrary transformed secrets, including JSON/shell escaping, are not comprehensively tracked by substring replacement. These additional paths are source-level observations, not separately counted reproductions.

**Remedy:** route every diagnostic through one redacting output layer. Preserve secret references/metadata in replay state and resolve/register them on every invocation. Keep sensitive payloads out of diagnostic strings wherever possible. Test rollback, nested exceptions, transformed credentials, and response echoes. Plaintext-at-rest deployment config is a documented design choice; the finding here is loss of output protection on replay, not an undisclosed claim that state is encrypted.

### A10 — HTTP redirects send custom authentication headers to another origin

Locations: [src/engine/builtins/http.rs:35](/Users/inou/dev/rust/nrgize-rs/src/engine/builtins/http.rs:35), [src/engine/builtins/http.rs:59](/Users/inou/dev/rust/nrgize-rs/src/engine/builtins/http.rs:59), [lib/bunny.rhai:77](/Users/inou/dev/rust/nrgize-rs/lib/bunny.rhai:77).

The agent uses default redirect handling and applies arbitrary caller headers. ureq's special authentication-header policy concerns `Authorization`; it does not classify Bunny's `AccessKey` as authentication. [ureq redirect policy](https://docs.rs/ureq/3.3.0/ureq/config/enum.RedirectAuthHeaders.html).

A loopback server redirected from one host/port origin to another, and the second server received `AccessKey: AUDIT_ONLY_PASSWORD`. This also occurs on the real GET path used in dry-run. Exploitation requires a redirect from a trusted/requested endpoint, an open-redirect opportunity, or an explicitly configured alternate API endpoint; it is not an arbitrary remote attack against TLS on api.bunny.net.

**Remedy:** disable redirects on authenticated API requests, or validate each redirect's scheme/host/port and explicitly strip all credential-bearing headers before crossing origins. Require HTTPS for production Bunny endpoints, with a separate intentional testing override.

### A20 — Group-writable files are an explicit remaining trust boundary

Locations: [src/trust.rs:41](/Users/inou/dev/rust/nrgize-rs/src/trust.rs:41), [src/trust.rs:56](/Users/inou/dev/rust/nrgize-rs/src/trust.rs:56), [src/engine/secret.rs:157](/Users/inou/dev/rust/nrgize-rs/src/engine/secret.rs:157).

The trust predicate requires matching UID and rejects world-write, but deliberately accepts group-write. A different member of a shared group can modify an owner-owned 0664 secrets file in place, preserving the UID check. A `CMD[...]` value then executes under the deploying account. ACL grants and writable symlink-path components are not comprehensively evaluated either.

This is a **documented policy tradeoff**, not an assertion that the code accidentally forgot its intended check. It is secure only if every effective writer is trusted with deploy-user privileges. That premise often fails on shared CI/build hosts.

**Remedy:** provide a strict production trust policy that rejects group-write and unsafe ACL/path access, using descriptor-based checks where appropriate. Permit shared writable workspaces only as an explicit trust choice. State clearly that ownership alone does not establish exclusive control.

## Robustness and operational inconsistencies

### A11 — Large values deadlock age subprocesses

Locations: [src/secrets/mod.rs:335](/Users/inou/dev/rust/nrgize-rs/src/secrets/mod.rs:335), [src/secrets/mod.rs:392](/Users/inou/dev/rust/nrgize-rs/src/secrets/mod.rs:392).

`encrypt_value` and `decrypt_value` synchronously write all input before `wait_with_output` drains stdout/stderr. Once pipe buffers fill, streaming age can block writing output while nrg blocks writing input. The main command runner already has a concurrent-pipe fix, but these sibling paths do not use it.

A real 1 MiB encryption input blocked beyond the five-second test deadline and required killing the isolated process group. Direct age, with simultaneous pipe draining, completed the same input in 0.018 seconds. Encryption was reproduced; decryption has the same source pattern but was not separately exercised with a large value.

**Remedy:** use one shared concurrent I/O subprocess implementation, propagate stdin-write errors, and add bounded large-input encrypt/decrypt tests.

### A12 — Responsive but stuck commands can hold a deployment indefinitely

Locations: [src/engine/runner.rs:315](/Users/inou/dev/rust/nrgize-rs/src/engine/runner.rs:315), [src/engine/runner.rs:331](/Users/inou/dev/rust/nrgize-rs/src/engine/runner.rs:331), [src/engine/runner.rs:76](/Users/inou/dev/rust/nrgize-rs/src/engine/runner.rs:76), [src/engine/builtins/exec.rs:157](/Users/inou/dev/rust/nrgize-rs/src/engine/builtins/exec.rs:157).

SSH connect timeouts and keepalives detect connection trouble, but do not bound a remote command on a responsive host. Local build/secret-manager calls and several Caddy curl calls also have no execution deadline. Rhai's interrupt check only runs between native calls. Captured output grows in memory without a project-defined bound, and fan-out creates a thread/process per host without a concurrency cap.

**Remedy:** configurable per-operation and deployment deadlines, cancellation-aware subprocess groups, bounded/streamed output, and bounded fan-out. Preserve sufficient output for diagnostics without risking OOM and loss of compensation. Distinguish a timed-out request's unknown remote outcome from a confirmed non-effect. The existing second-signal escape hatch permits forced exit but cannot make the remote deployment consistent.

### A13 — Distributed locking is not interruption-safe or overlap-safe

Locations: [lib/deploy.rhai:258](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:258), [lib/deploy.rhai:594](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:594), [lib/deploy.rhai:1299](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:1299), [src/engine/mod.rs:78](/Users/inou/dev/rust/nrgize-rs/src/engine/mod.rs:78).

Signal termination raises Rhai `ErrorTerminated`, which bypasses script `try/catch`. Remote-lock release is in those catch/normal paths, not a Rust lifetime guard. Reproduced: one SIGTERM during the fake pull aborted nrg and left the acquired distributed-lock marker present.

Lock placement also depends on the alphabetically first supplied host. `[web1, web2]` and `[web2]` lock different machines while both mutate web2. Host aliases can similarly diverge. The lock therefore serializes an identical canonical fleet, not every overlapping deployment of the service. Different machines also retain independent local state files even when a remote lock serializes them; a lock alone does not reconcile old targets.

**Remedy:** acquire/release locks through interruption-safe native orchestration, use a stable coordinator or ordered per-host locks with ownership tokens, and define a shared/reconciled state source. Do not automatically delete a lock merely because it is old without checking ownership/liveness. Add interruption tests around every phase and concurrent overlapping-fleet tests.

### A14 — Remove can race a deployment and leaves stale state/secrets after purge

Locations: [src/cli/remove.rs:49](/Users/inou/dev/rust/nrgize-rs/src/cli/remove.rs:49), [src/cli/remove.rs:94](/Users/inou/dev/rust/nrgize-rs/src/cli/remove.rs:94), [src/cli/remove.rs:189](/Users/inou/dev/rust/nrgize-rs/src/cli/remove.rs:189).

`nrg remove --yes` takes neither the project state lock nor the distributed service lock. It can remove the canonical old container while a concurrent deploy expects to restore traffic to it. Its state writes also participate in unprotected read-modify-write sequences.

`--purge-state` deletes `.target` and four global fields but leaves `.port` and `.config`; it ignores every deletion error. Reproduced: a successful full purge retained `app.port.web1` and the complete persisted config. The port can subsequently be trusted as an old route even though removal destroyed the container, and config can retain credentials after an operator expects the service's state to be gone.

**Remedy:** run removal under the same local/remote coordination as deploy, delete the complete defined per-service/per-host schema with checked errors and an atomic state update, and report residual state accurately. Retaining the proxy route is explicitly warned about today; that is a documented behavior, separate from the stale-state defect.

### A15 — Native rollback uses Docker for Podman/nerdctl deployments

Locations: [src/engine/eval.rs:175](/Users/inou/dev/rust/nrgize-rs/src/engine/eval.rs:175), [lib/runtime.rhai:98](/Users/inou/dev/rust/nrgize-rs/lib/runtime.rhai:98), [lib/deploy.rhai:590](/Users/inou/dev/rust/nrgize-rs/lib/deploy.rhai:590).

Native `nrg rollback` does not run the orchestration file's runtime setup. The Rhai runtime resolves from session state and defaults to Docker; the persisted `nrg.runtime.cmd` is not copied into that session. Reproduced with saved Podman state: rollback planned `docker pull`, Docker proxy commands, and Docker container operations.

This can fail during incident recovery, or operate on a different daemon when multiple runtimes coexist. Runtime is also stored globally per destination rather than per service, while several native commands assume the global value is correct for every service.

**Remedy:** persist validated runtime selection per service/host and explicitly initialize native rollback's session from it, with explicit overrides. Test the native command end to end for each supported runtime.

### A16 — Named destinations are unavailable to day-2 commands

Locations: [src/cli/status.rs:13](/Users/inou/dev/rust/nrgize-rs/src/cli/status.rs:13), [src/cli/logs.rs:12](/Users/inou/dev/rust/nrgize-rs/src/cli/logs.rs:12), [src/cli/app.rs:24](/Users/inou/dev/rust/nrgize-rs/src/cli/app.rs:24), [src/cli/remove.rs:25](/Users/inou/dev/rust/nrgize-rs/src/cli/remove.rs:25), [src/cli/lock.rs:90](/Users/inou/dev/rust/nrgize-rs/src/cli/lock.rs:90).

Deploy/run/rollback support `--dest`; status/logs/app exec/remove/lock/doctor do not select a destination and load the unnamespaced store. A service deployed only to production/staging is invisible to host discovery. Passing an explicit host can bypass discovery while still using the wrong runtime/state namespace.

**Remedy:** provide consistent destination selection across all state-consuming CLI commands, include the effective destination in output and audit events, and test a project with the same service name in multiple destinations. Namespace remote resources deliberately too when destinations share hosts; local state namespacing alone does not distinguish `<service>-web` containers and proxy route IDs.

### A17 — Dry-run loses project-root identity for secret lookup

Locations: [src/engine/state.rs:189](/Users/inou/dev/rust/nrgize-rs/src/engine/state.rs:189), [src/engine/secret.rs:219](/Users/inou/dev/rust/nrgize-rs/src/engine/secret.rs:219), [src/engine/secret.rs:70](/Users/inou/dev/rust/nrgize-rs/src/engine/secret.rs:70).

`load_overlay` sets `root = None` to disable persistence. Secret lookup also interprets this field as the project identity, and falls back to paths relative to CWD. From a project subdirectory, a live invocation finds the root secret while the same dry-run reports it missing; if the subdirectory contains a different secret file, it can select that instead.

This was reproduced with one root `.energize/secrets` file and the same script invoked from `sub/`: live succeeded, dry-run failed. CMD fetches still execute in dry-run, so a wrong lookup can also select a different actual local fetch command.

**Remedy:** separate immutable project-root identity from the persistence-enabled flag. Separately define the working-directory contract for relative `build_context`: finding the script at the project root does not currently chdir the local build to that root.

### A18 — HTTP body read failures are reported as successful empty responses

Location: [src/engine/builtins/http.rs:69](/Users/inou/dev/rust/nrgize-rs/src/engine/builtins/http.rs:69).

After receiving response headers, `finish` uses `read_to_string().unwrap_or_default()` and retains the successful status. Truncated bodies, decoding errors, body-size limits, and read timeouts can therefore be collapsed into `{status: 200, body: ""}`. A Bunny create path explicitly accepts an empty successful body as an empty map.

Reproduced with a server advertising 100 bytes and closing after three: nrg returned status 200 and an empty body. Health checks that intentionally inspect status only have a different contract; general API calls need the error distinction.

**Remedy:** propagate body-read failures as transport/body errors with context. Give status-only probes an explicit implementation that does not accidentally hide general API failures.

### A19 — Fresh Caddy provisioning fails before starting the proxy

Locations: [lib/caddy.rhai:75](/Users/inou/dev/rust/nrgize-rs/lib/caddy.rhai:75), [src/engine/eval.rs:305](/Users/inou/dev/rust/nrgize-rs/src/engine/eval.rs:305).

The boot path writes `/etc/caddy/caddy.json` without creating `/etc/caddy`. Neither setup's generated script nor `write_remote` creates its parent. A fresh Docker host usually does not have a host-side Caddy configuration directory; pulling a Caddy image does not create it. Even a root SSH user cannot redirect into a missing parent, and an ordinary deploy user additionally needs a deliberate permission/provisioning strategy.

**Remedy:** provision the required directory safely with the intended ownership, or initialize configuration through a managed volume/container mechanism. Add a fresh-host integration test. Also wait for the Caddy admin API before attempting the first route update: `docker run -d` success is not an API-readiness signal.

### A21 — Bunny's canary and rollback guarantees need narrowing or stronger checks

Locations: [lib/bunny.rhai:231](/Users/inou/dev/rust/nrgize-rs/lib/bunny.rhai:231), [lib/bunny.rhai:263](/Users/inou/dev/rust/nrgize-rs/lib/bunny.rhai:263), [lib/bunny.rhai:294](/Users/inou/dev/rust/nrgize-rs/lib/bunny.rhai:294), [lib/bunny.rhai:565](/Users/inou/dev/rust/nrgize-rs/lib/bunny.rhai:565).

`wait_for_image` checks the app configuration's `containerTemplates[].imageTag`, not running instance readiness or the deployed digest. The optional health URL can still answer from the previous version while the desired config already contains the new tag. Without a health URL, a canary has no application-health gate at all. Consequently “verified canary” should not be read as proof that the new workload is serving correctly. Bunny documents application/region/pod health separately from configuration. [Bunny troubleshooting](https://docs.bunny.net/docs/magic-containers-troubleshooting-application).

Rollback snapshots only the tag, despite deploy accepting an image name and digest. A change from image A:v1 to B:v2 cannot be reversed to A:v1 with a patch that only restores `imageTag: v1`; digest-pinned state is also not restored. The tag-only limitation is already documented. Failed/repeated attempts overwrite the single predecessor before success, and there is no cross-machine Bunny rollout coordination or automatic fleet compensation.

**Remedy:** separate “configuration accepted” from “new version running.” Require a version/digest-aware readiness gate before advancing a production canary, and snapshot/restore the complete changed image identity. Define the partial-fleet recovery contract explicitly. These are source-confirmed limitations; propagation timing and full restoration were not tested against a live Bunny account, so an account-specific incident is not claimed.

## Lower-priority gaps

### A22 — Bunny cannot be obtained through the normal embedded/vendor flow

Locations: [src/engine/stdlib.rs:23](/Users/inou/dev/rust/nrgize-rs/src/engine/stdlib.rs:23), [src/cli/vendor.rs:39](/Users/inou/dev/rust/nrgize-rs/src/cli/vendor.rs:39).

The embedded catalog lists nine modules but omits `bunny`, and `nrg vendor` materializes only that catalog. `import "std/bunny"` fails; the documented `import "lib/bunny"` requires manually copying the module even after vendoring. Reproduced the missing embedded import. Add Bunny with its dependencies to the catalog and test it in an empty project and a freshly vendored project.

### A23 — Audit history is incomplete and sometimes mislabeled

Locations: [src/audit.rs:53](/Users/inou/dev/rust/nrgize-rs/src/audit.rs:53), [src/audit.rs:87](/Users/inou/dev/rust/nrgize-rs/src/audit.rs:87), [src/cli/audit.rs:55](/Users/inou/dev/rust/nrgize-rs/src/cli/audit.rs:55), [src/cli/setup.rs:155](/Users/inou/dev/rust/nrgize-rs/src/cli/setup.rs:155).

Append errors are silently ignored; malformed/read-failed logs become omitted entries or empty history. Remove, manual locks, app exec, and secrets operations do not pass through the audit writer. Setup may install Docker before entering audited execution; an installation failure can therefore be absent. The renderer derives “run/exec” from target presence rather than `entry.command`, so rollback/setup are not faithfully identified. README's “every invocation” description is too broad.

Warn on audit-write failure, render the stored command accurately, and record begin/end/failure events for relevant mutations. Offer an external append-only sink if incident/compliance evidence is required. The current same-user local log is useful operational history, not a tamper-resistant security audit system. Its terminal-control escaping is an improvement already present.

### A24 — CI and release hardening do not yet match production claims

Locations: [.github/workflows/ci.yml:23](/Users/inou/dev/rust/nrgize-rs/.github/workflows/ci.yml:23), [.github/workflows/release.yml:14](/Users/inou/dev/rust/nrgize-rs/.github/workflows/release.yml:14), [scripts/install.sh:115](/Users/inou/dev/rust/nrgize-rs/scripts/install.sh:115), [homebrew/nrg.rb:27](/Users/inou/dev/rust/nrgize-rs/homebrew/nrg.rb:27), [tests/secrets.rs:60](/Users/inou/dev/rust/nrgize-rs/tests/secrets.rs:60).

CI tests Linux only; release builds include macOS but do not run the suite there. The observed `echo -n` failure demonstrates the gap. Formatting is not enforced, toolchain/MSRV are not pinned/declared, and subprocess-heavy tests have no suite-wide per-test deadline. The release gate repeats tests/clippy but omits the dependency audit. Dependency unmaintained notices currently do not block release.

Workflow actions use mutable version tags. Release workflow `contents: write` is applied to all jobs rather than only publication. Release archives have checksums but no independently verified signing/provenance in the installer; downloading the checksum from the artifact's own origin provides integrity against corruption, not independence from a compromised release account. The install destination temporary filename uses `$$` and non-exclusive `cp`, another shared-writable-directory hazard. The checked-in Homebrew formula is explicitly a placeholder; no conclusion is drawn about the separate live tap.

Use a Linux/macOS test matrix, portable fixtures, explicit timeouts, release audit gates, read-only default workflow permissions with scoped publication rights, pinned action revisions, and verified release provenance/signatures appropriate to the distribution model. Use unique exclusive destination temporary files for installation. Track Rhai's unmaintained dependency upstream. These are hardening gaps, not evidence that CI or release assets are compromised.

## What is already working well

- SSH engine/log connections default to `StrictHostKeyChecking=yes`, pass host aliases through to OpenSSH, reject leading-option hosts, and use connection timeouts/keepalives.
- Shell quoting is centralized at important boundaries; registry-password and env-file bodies are sent over stdin rather than argv.
- New state files use exclusive temporary files, fsync, and atomic rename with 0600 permissions. Corrupt/future state is refused rather than silently reset. Named-state key boundaries have tests.
- The transaction engine runs LIFO, isolates compensation errors, and consumes interrupts so cleanup callbacks can execute. The failures above lie in orchestration decisions and lifecycle coverage around those primitives.
- Dry-run intercepts mutating builtins and uses an in-memory state/simulation model. Its explicit live probes and secret fetches mean it is not an untrusted-script sandbox or a fully offline mode.
- Default application port publishing is now loopback-only. Root build-context exclusions prevent normal remote sync from copying the project's `.nrg-key`, `.energize`, and `.env` entries.
- Current audit display escapes terminal/bidirectional controls; raw stdout/stderr/log-stream paths do not all share that protection.
- Tests cover many previously identified injection, error-classification, state, dry-run, and rollback regressions. They do not establish crash-safe distributed atomicity or live proxy/runtime compatibility by themselves.

## Documentation corrections

Several claims should be narrowed while fixes are pending:

| Claim | Current behavior |
|---|---|
| Fleet is never left half deployed / fleet-atomic | Compensation can fail; uncertain switches, interrupts, and post-commit state gaps need reconciliation |
| Caddy domain routing implies secure HTTPS ingress | Matching routes are also served on HTTP (A04) |
| `write_remote` produces a 0600 file | Only reliably true for a newly created non-symlink path (A06) |
| Secrets cannot appear in output | Native compensation errors and config replay can leak them (A09) |
| Dry-run has zero side effects | Explicit probes, real HTTP GETs, and CMD secret fetches still execute; SSH may also update local connection/trust artifacts under chosen settings |
| HTTP GET/POST both return synthetic 200 in dry-run (`docs/safety.md` table) | GET probes live; write verbs are simulated |
| State backup uses fixed `state.json.tmp` | Current implementation uses unique temporary files; backups are per write, not a consistent pre-deploy snapshot |
| `--env-file` hides credentials from `docker inspect` (`docs/deploy.md`) | It avoids command-line values; container environment is still available through runtime inspection to principals with runtime access |
| Audit history covers every invocation | Several commands and early failures do not produce audit records (A23) |
| One destination's state guarantees full environment isolation | Remote container/route names still derive from service, and day-2 commands omit destination selection (A16) |

Intentional raw-shell script APIs, mutable image tags other than `latest`, explicit `NRG_SSH_HOST_KEY_CHECKING=no/off`, and shared-group write access should be presented as trust choices. Refusing only `latest` does not make arbitrary other tags immutable; a digest is the reliable image identity to record and verify.

## Recommended remediation order and acceptance criteria

1. **Traffic safety and rollback state:** A01–A03. Add a durable per-host transition record, preserve predecessor history on all failures, distinguish unknown outcomes, and reconcile before destructive cleanup. Inject lost acknowledgments and failures at every effect/state boundary; verify the only serving container is never removed.
2. **Default blast radius and ingress:** A04–A05. Verify HTTP redirects, HTTPS access, and service-scoped cleanup on actual runtimes. Keep unrelated stopped containers intact.
3. **Credential lifecycle:** A06–A10. Secure temporary files and unseal publication; centralize redaction; preserve secret identity across replay; constrain authenticated redirects. Test existing modes, symlinks, late ciphertext corruption, callback errors, and cross-origin redirects.
4. **Operational coordination:** A11–A19. Bound subprocesses, fix lock release and overlap, coordinate removal, repair replay runtime/destination behavior, preserve overlay root identity, and surface HTTP read errors. Add real fresh-host smoke tests for each supported runtime/proxy combination.
5. **Threat model and release quality:** A20–A24. Decide shared-user trust policy, define Bunny readiness/restoration semantics, finish module distribution, improve audit fidelity, and enforce cross-platform release gates.

Do not treat a green unit suite or the clean vulnerability count as acceptance for the High findings. Their acceptance tests must exercise the failure transitions and real proxy semantics that current tests miss.

## Reproduction artifacts

- [reproduce.py](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/reproduce.py): 15 isolated CLI/filesystem/HTTP defect assertions. No actual SSH connections.
- [external_checks.py](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/external_checks.py): real age/Caddy probes using supplied binaries.
- [reproduction-results.log](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/reproduction-results.log), [external-results.log](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/external-results.log): observed outcomes.
- [tests-final.log](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/tests-final.log), [clippy.log](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/clippy.log), [fmt.log](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/fmt.log): verification output.
- [dependencies.json](/Users/inou/dev/rust/nrgize-rs/docs/audit-2026-09-05/dependencies.json): complete cargo-audit result and database identity.

Run the first script with `python3 reproduce.py /absolute/path/to/nrgize-rs` after `cargo build --locked`. Run the second with `python3 external_checks.py /absolute/path/to/nrgize-rs /path/to/test-tools/bin`. The supplemental directory must contain age, age-keygen, and Caddy. Assertions intentionally describe vulnerabilities at the audited revision; they should fail after fixes and be converted into regression tests asserting safe behavior. Scripts retain temporary fixture/evidence directories and print their locations. All passwords are dummy audit strings.
