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
