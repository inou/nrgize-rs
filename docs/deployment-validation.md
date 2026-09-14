# Deployment safety validation (2026-09-14)

All work and tests were local. LibrusBridge's `Energize.rhai` was read as a reference;
no application was deployed and no production SSH connection was made.

## Checks executed

- `cargo build --all-targets --locked`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `CI=true cargo test --all-targets --locked --no-fail-fast`: first exposed sandbox
  restrictions on loopback fixtures, unavailable GNU rsync/age, and test parsers that
  needed to strip the new plan annotation. The parser failures were fixed.
- The full suite with loopback access and only the two unavailable-tool gates excluded
  reported **747 passed, zero failed**. Existing age-dependent tests self-skip when age is
  absent; their passing status does not establish age coverage. This run preceded the
  final escaped-secret and upload-length hardening; those changes were verified by the
  focused tests below.
- The final deployment safety target reports **15 passed**: 13 run by default, plus two
  tool-dependent cases that report unavailable without the corresponding tools/settings.
  The real mise case was also executed separately with explicit installed versions.
- Targeted doctor, audit, dry-run, embedded stdlib, exec, runner, diagnostics, Caddy shell,
  and multi-architecture shell tests passed.
- Four intentional mutations (wrong destination mode, inverted temporary-check opt-in,
  removed chunk-prefix retention, and Elixir before Erlang) each caused the intended
  regression test to fail. All mutations were restored.

## Real tools versus fixtures

| Coverage | Result |
| --- | --- |
| macOS `/usr/bin/rsync` (openrsync, protocol 29) | Actual copies passed: new file and existing `0666` destination both become `0600`; content verified |
| GNU rsync 3 | Unavailable locally. `NRG_TEST_GNU_RSYNC` selects an installed binary. CI installs rsync and requires this test |
| SSH file transfer shell commands | Actual local shell, binary file I/O, chmod, mktemp, byte-count checks and rename; SSH is replaced by a local-shell transport fixture |
| File replacement failures | Truncated upload, failed chmod, failed download, symlink destination, and invalid mode tested; original destination and cleanup checked |
| Streaming | Actual processes fill both pipes, split secrets across writes, and wait for a test-controlled gate proving output arrives before completion |
| mise verification | Actual installed mise with Erlang `28.5.0.6` and Elixir `1.20.4-otp-28`; explicit execution works without shell activation or tool installation |
| mise provisioning | FakeRunner ordering tests plus a shell installer fixture checking persisted configuration, failures, and repeat execution |
| age | Unavailable locally; encrypted-secret integration coverage remains unverified |

The CI matrix uses Linux and macOS 15, with system openrsync on macOS and installed
rsync 3. CI itself was not run from this workspace.

## Remaining limits

Fresh Erlang/Elixir downloads and compilation, actual SSH network transfer, remote OS
capabilities, and GNU rsync behavior were not verified locally. Probe the intended host
and filesystem explicitly before deployment. A passing parser or dry run is not such a
probe. Registered-secret redaction covers raw, JSON-escaped, and shell-quoted forms per
stream, not arbitrary transformations or unknown secrets. Atomic replacement assumes a
trusted parent directory; power loss, SIGKILL, or network loss can defeat remote cleanup.

## General workflow follow-up

The subsequent general-purpose workflow changes passed the final local gates:

- `cargo build --all-targets --locked`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- Full local suite with loopback fixture access: **766 passed, zero failed**.
  `real_gnu_rsync_permissions` and the CI age-presence gate were explicitly excluded
  because GNU rsync and age are unavailable. Existing age-dependent tests still
  self-skip; the count does not imply encrypted-secret coverage.
- That full run explicitly enabled actual preinstalled mise verification with
  Erlang `28.5.0.6` and Elixir `1.20.4-otp-28`. No tools were installed.
- The new `general_workflows` target contributes **16 integration tests** using
  actual local child processes, signals and files. SSH transport is replaced by a
  local-shell fixture. It covers default/release starter dry runs, options and
  binary stdin, argv/log redaction, retry guards, FIFO rejection, timeouts and
  temporary cleanup, prompt newline-free output, cancellation/compensation,
  cancellation during owned-lock acquisition, crash/incomplete journal records,
  torn-tail recovery, journal permissions, status JSON/exit behavior, directory
  deployment failures/rollback, path guards, and framework config overrides.
- Seven intentional mutations each failed their targeted test: retry idempotence,
  nonblocking FIFO open, status classification, release restoration, output flush,
  process cancellation, and interrupt-safe lock cleanup. All were restored.

macOS openrsync copies ran again in the full suite. Directory release switching
used real macOS shell/filesystem tools (including the BSD `mv` fallback). Linux/GNU
release switching is covered by the CI matrix but was not executed locally. The
framework recipes were exercised as Rhai configuration and orchestration helpers;
no Rails, Django, Next.js, Phoenix or Laravel application was built or deployed.
No real SSH server or production host was contacted. The new journal records
instrumented operations, not arbitrary shell semantics, and does not implement
automatic resume or database rollback. See [workflows](workflows.md).

## CI and Homebrew follow-up

The macOS CI run on `bf94425` failed because the installer test's temporary Python
HTTP server never became reachable. The fixture now owns an IPv4 loopback listener
in-process, with bounded socket/installer timeouts and shutdown on assertion
failure. It still uses real curl, tar, checksum verification, and executable
installation. Repeat installation and corrupt-upgrade preservation are asserted.

- The complete local suite now passes **768 tests with no exclusions**, after
  installing age 1.3.2 and GNU rsync 3.5.0. This supersedes the local tool gaps
  above: both real rsync variants and age encryption tests executed. Preinstalled
  mise verification used the same explicit Erlang/Elixir versions as above.
- Build, clippy with warnings denied, formatting, workflow YAML parsing, and both
  Python audit suites passed. The external suite used actual age and Caddy 2.8.4
  against temporary files and loopback ports.
- Temporarily bypassing installer checksum verification caused the corrupt
  upgrade assertion to fail with unexpected success. The mutation was restored
  and all nine installer tests passed again.
- Homebrew 7.0.1 reproduced an untrusted-formula error while adding the explicit
  tap URL. Trusting only `inou/nrg/nrg` before tapping fixed it. Actual installation
  of the checksum-pinned macOS ARM64 0.1.3 archive and `brew test inou/nrg/nrg`
  passed. Documentation and the Homebrew workflow now handle this ordering.
- The release test job now matches CI's macOS 15 runner and age/Caddy/rsync
  dependencies. No release tag was created; publishing and the other three
  release archive platforms were not exercised locally.

These checks do not establish fresh mise installation, real SSH transport, or
production deployment compatibility. No application was deployed or production
server modified. Remote CI results are separate from these local results.

## Recipe contracts and rehearsal

The new additive `nrg rehearse` command and `std/contracts` helpers passed:

- `cargo build --all-targets --locked`, clippy with warnings denied, and formatting.
- The final full suite after restoring every mutation: **782 passed, zero failed,
  no exclusions**. Real age, both macOS openrsync and GNU rsync, and the explicitly
  selected preinstalled mise toolchain remained enabled.
- Thirteen new integration tests cover declaration-only behavior with an unusable
  temporary directory, rejected execution/file imports, evaluation limits, complete
  schema validation before setup, first/repeat/restart ordering, exact fault exits,
  recovery checks, failure/cleanup propagation, timeout, SIGINT, fresh environments,
  chunk-spanning secret redaction, concurrent large streams, and workspace removal.
- The real HTTP example uses Python 3, an assigned IPv4 loopback port, actual
  process restart/termination, HTTP 503 rollback, and interruption of a real partial
  file write. It runs in the integration suite on both CI platforms.
- Nine mutations each failed the intended regression: execution opt-in, fault
  opt-in, exact expected exit, cleanup after failure, cleared child environment,
  repeat deployment, failure propagation, mandatory postconditions, and reintroducing
  reverse hostname lookup in the HTTP fixture. All were
  restored before the final full run.
- Four Rhai blocks in the new guide parsed; its three complete contracts also
  passed declaration validation. Local documentation link targets were checked,
  and the guide rendered to standalone HTML with Pandoc.

The reference HTTP fixture is not a production deployment implementation. No
framework application was built, no new runtime was provisioned, and no actual
SSH transport was exercised by these new tests. Rehearsal workspaces are not OS
sandboxes: trusted commands retain the user's permissions, and adapters own
cleanup of external resources. No application deployment or production-server
modification was performed. Existing installed release binaries do not gain the
new command until a release containing it is published.

The first remote macOS run exposed a readiness failure in the reference HTTP
fixture. Its server now binds without Python HTTPServer's reverse hostname lookup;
a regression disables that lookup and runs the complete real HTTP contract.
Temporarily restoring the default server fails specifically at the injected DNS
error. Failed integration runs now print the failed step records directly, so the
assertion library cannot truncate their diagnostic excerpts out of a large report.

A subsequent full run exposed an intermittent installer-fixture connection reset
on macOS. Accepted sockets can inherit the listener's nonblocking mode on BSD;
the fixture now explicitly selects blocking reads with a timeout. A new test waits
for acceptance before sending delayed, fragmented headers: it reproduced a broken
pipe before the fix and passed afterward. The final total above includes this
additional installer regression. See Apple's [accept manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/accept.2.html).
