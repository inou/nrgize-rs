# September 5 audit remediation

This change addresses all 24 findings with implementation changes or explicit limits.
The original REPORT.md and vulnerable-revision reproduction artifacts are preserved as
evidence. This is not a claim of crash-atomic distributed transactions or live fleet certification.

| Finding | Remediation | Remaining boundary |
|---|---|---|
| A01 | Mark switch attempted before dispatch; register container compensation before creation; preserve backend and journal if restoration fails. | Unknown outcomes require manual reconciliation. |
| A02 | Commit release/config/history/runtime/host targets in one atomic update before cleanup. Journal each host transition; stop dependent cleanup after rename failure. | No automatic remote reconciliation; crashes during transitions require inspection. |
| A03 | Update predecessor only in the successful release batch; retain predecessor configuration for rollback. | History remains one predecessor, not a release database. |
| A04 | HTTPS-only application listener; migrate running/autosaved Caddy listeners using the admin API. | Existing hosts receive migration on the next proxy boot/deploy invocation. |
| A05 | Deploy no longer calls unscoped container/image pruning. Container stop/remove preserve daemon errors except confirmed absence. | Explicit `docker_cleanup` remains a host-wide operator API; image retention remains opt-in. |
| A06 | Exclusive private env directories, fresh 0600 remote files, atomic replacement, env cleanup after consumption and write failure. | Trusted parent directories required; lost transport can leave private temporary artifacts. |
| A07 | OS-created local archives and unique remote build directories replace predictable tag-derived files. | Interrupted transfers can leave private remote directories for cleanup. |
| A08 | Decrypt to a private tempfile; publish atomically with no-clobber or force semantics after authentication succeeds. | Parent directory is trusted. |
| A09 | Redact compensation/trace diagnostics, common JSON/shell encodings, and legacy config strings during native replay. | Arbitrary transformations cannot be discovered by substring redaction; state still contains plaintext. |
| A10 | Disable HTTP redirects, including requests with custom credentials; Bunny non-loopback endpoints require HTTPS. | Intentional loopback test endpoints permit HTTP. |
| A11 | Concurrent age stdin/output handling and propagated input errors. | Age remains an external executable. |
| A12 | Configurable command deadline/output bound; Unix process-group termination; 16-operation bulk concurrency cap. | No total deployment deadline or first-signal preemption of every blocking native call. Streaming interactive commands have a different contract. |
| A13 | Ordered per-host locks with ownership tokens; native lifetime cleanup survives Rhai interruption. Remove uses the same locks. | Duplicate aliases within one fleet, independent controller state, SIGKILL, and unavailable hosts require operator coordination. |
| A14 | Local/project and remote locks surround removal; checked atomic purge covers the defined service/host schema and sanitizes the automatic backup. | Proxy routes remain explicitly managed separately. Historical external backups are outside the state store. |
| A15 | Save runtime per service; native rollback initializes the runtime session; status/logs/app/remove prefer service runtime. | Old state uses the global compatibility fallback. |
| A16 | Global `--dest` available to status/logs/app/remove/lock/doctor, with destination diagnostics and mutation audit metadata. | Remote names still derive from service. Use distinct service names for destinations sharing hosts. |
| A17 | Overlay retains project root independently of persistence. | Relative builds deliberately use invocation CWD. |
| A18 | Body failures report status 0 and a body-read diagnostic rather than successful empty responses. | Status 0 covers transport and incomplete response bodies. |
| A19 | Provision `/etc/caddy` explicitly, wait for admin readiness, then enforce listener configuration. | SSH user needs directory provisioning rights; no live fleet provisioning was performed. |
| A20 | Optional `NRG_STRICT_TRUST=1` rejects group-write. | Compatibility default trusts group members; complete ACL/descriptor/path-race enforcement remains open. |
| A21 | Preserve full changed image identity; commit fleet predecessor snapshots after verification; avoid overwriting history on no-op retries. Optional `require_version_health` checks exact application revision. | Default polling confirms configuration only. No distributed Bunny lock or automatic fleet compensation; live provider behavior remains unverified. |
| A22 | Embed/vendor Bunny alongside its dependencies. | None identified for catalog delivery. |
| A23 | Warn on audit read/parse/append failures, render actual command, add mutation begin/end events, observe app subprocess exits. | Same-user logs are editable; abnormal termination can omit end events. No external append-only sink is configured. |
| A24 | Linux/macOS gates, real age/Caddy regressions, formatting, pinned Rust/MSRV and action SHAs, scoped permissions, release dependency audit/provenance, optional installer provenance verification, exclusive installation tempfiles. | CI/release changes have not executed on GitHub here. Per-test suite deadlines and the placeholder Homebrew formula still need distribution decisions. Rhai's unmaintained transitive dependency remains an upstream warning. |

## Bunny readiness contract

`wait_for_image` observes desired configuration. A successful status-only health endpoint
can still be served by the previous revision. For a production rollout, set
`require_version_health: true` on `deploy_fleet` and supply a `health_url` for every target.
The endpoint body, trimmed, must equal the requested image digest, or the image tag when
no digest was supplied. Serve that value from the running process; do not return a
control-plane desired revision. Immutable digests are preferable to mutable tags.

A failed fleet rollout can leave successful targets upgraded. Inspect the returned/printed
failed target set and explicitly roll back or complete the remaining targets. Image identity
restoration does not restore database schemas or provider fields outside the changed image.

## Verification

Local validation used Linux, Rust 1.95.0, age 1.2.1, and Caddy 2.6.2.
The Rust suite passed 736 tests across 39 targets; Clippy and formatting passed.
The Python suites passed 22 CLI/filesystem/HTTP fault checks and three real-tool checks
(including large-value encryption/decryption and legacy Caddy listener migration).
Some pre-existing platform/privilege-specific tests can self-skip. `cargo-audit` was
not rerun locally; its CI and release gates are configured, and the upstream dependency
warning above is retained from the original report.

- `cargo test --all-targets --locked --no-fail-fast` with real age available.
- `cargo clippy --all-targets --locked -- -D warnings`.
- `cargo fmt --all -- --check`.
- `python3 tests/audit_regressions.py "$PWD"`: real CLI, isolated fake SSH, local files and loopback HTTP fault cases.
- `python3 tests/audit_external_regressions.py "$PWD" /usr/bin`: real age large-value/corruption tests and real Caddy HTTP redirect, HTTPS response, and legacy-listener migration.

The Python fixtures intentionally retain evidence under temporary directories. They use
dummy credentials and never connect to a deployment fleet. Live Docker/Podman/nerdctl,
macOS execution, Bunny account readiness, and release publication remain unverified locally.
