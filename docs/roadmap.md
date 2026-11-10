---
title: Product Roadmap
nav_order: 98
---

# Product roadmap

Everything that has shipped is recorded in [`CHANGELOG.md`](../CHANGELOG.md) and the
per-release notes under [`releases/`](releases/). This page lists only what is still open.

## Open items

- **Publish the crate to crates.io** so `cargo install nrg` works. A credentialed, one-way
  maintainer action; the crate is not published today (see
  [Installation](../README.md#installation)).
- **Deploy-aware history and version-targeted rollback.** `nrg audit` and the run journal record
  invocations and instrumented steps, not "service X went from image A to image B", so
  `nrg rollback` can only target the single `<service>.prev` snapshot or an explicit `--image`.
- **A global deadline and preemption covering `http_*` builtins.** `ssh_exec` / `local_exec` and
  named steps have deadlines (600 s default, `NRG_COMMAND_TIMEOUT_SECS`); `http_*` builtins have
  only their own per-request timeout (30 s default) and are not covered by that deadline.
- **Isolation backends and live framework fixtures for rehearsal.** The
  [rehearsal runner](rehearsal.md) provides first/repeat/restart scenarios, failure injection and
  JSON reports; VM/container isolation backends and tested framework application fixtures
  remain future work. The runner does not certify arbitrary recipes or infer shell semantics.
- **Per-destination container naming.** `--dest` isolates state only, so two destinations on one
  host still share a container name; give each destination its own host(s) (see
  [Environments / destinations](cli.md#environments--destinations)).
- **Automatic recovery and resume** of interrupted runs, and live framework deployment
  validation. Interrupted or ambiguous cutovers keep recovery journals and need manual
  reconciliation (see [Safety Features](safety.md)).
