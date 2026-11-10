# Changelog

All notable changes to `nrg` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may change behavior).

The full release notes for each tag live in [`docs/releases/`](docs/releases/).

## [Unreleased]

### Changed

- Documentation: brought the guides back in line with the code. Both reverse-proxy backends
  (`std/proxy` for kamal-proxy, `std/caddy`) are described, the concurrency note matches the
  tokio-free engine, user-facing examples import the embedded `std/X` stdlib (with one
  explanation of `std/` versus the vendored `lib/`), and the builtin and stdlib references now
  cover `state_update`, `nrg_dest`, `json_string`, `to_json` / `from_json`, `url_encode`,
  `to_debug`, the remote-lock builtins, `docker_promote`, `docker_run_once` and
  `db_hostname`.
- `scripts/install.sh` falls back from the musl to the glibc Linux archive only when the musl
  asset is missing (HTTP 404); any other download failure (server error, no network) now aborts
  with the real error instead of silently installing a different build. The static musl
  archives exist only from the release after v0.1.4 onwards.
- Error messages thrown by the stdlib (`std/deploy`, `std/docker`, `std/healthcheck`,
  `std/recipe`) and state-key errors no longer cite internal review/roadmap numbers.
- `nrg doctor --checks` help now says it takes the path to a JSON file; `docs/cli.md` documents
  `--dest` as the single global flag it is (before or after the subcommand; conflicting repeats
  are an error). The Homebrew formula bump PR is skipped for pre-release tags and is safe to
  re-run.

## [0.1.4] - 2026-09-26

General deployment workflows beyond containers. See [v0.1.4 notes](docs/releases/v0.1.4.md).

### Added

- Directory releases: `std/release` prepares a versioned directory, runs optional build and
  migration hooks, switches `current`, checks activation, and restores the previous release
  when activation or health fails. `nrg init --template release` scaffolds one. The optional
  `keep` setting prunes old releases after a successful deploy while always retaining the
  active release and its rollback target.
- Optional framework defaults in `std/release_recipes` (Rails, Django, Next.js, Phoenix,
  Laravel) and app-scoped toolchains in `std/mise`.
- `upload_file` / `download_file` with explicit permissions and byte-count checks;
  `ssh_step` / `local_step` named, journaled, redacted steps; preflight capability checks.
- `read_file(path)` for local text configuration (read-only, runs in dry-run) and an optional
  permissions argument for `write_remote`.
- Recipe contracts (`std/contracts`) and `nrg rehearse` for exercising real commands and
  recovery checks in a disposable local workspace.
- Durable run diagnostics and history (`nrg audit --run`, `--incomplete`, `--json`).

### Changed

- `write_remote` without permissions still writes `0600`; releases are never pruned unless
  `keep` is set.
- Installer and Homebrew tap fixes; the tap is served from this repository.

### Security

- Updated `rustls` to 0.23.45 for RUSTSEC-2026-0285.

## [0.1.3] - 2026-09-06

Re-release of 0.1.2. The `v0.1.2` tag did not publish a release because its macOS validation
gate rejected Linux-only Docker fixtures; 0.1.3 corrects their platform scope while keeping
mandatory real-container coverage on Linux. Contents are otherwise those of 0.1.2 below. See
[v0.1.3 notes](docs/releases/v0.1.3.md).

## [0.1.2] - 2026-09-06

Security and reliability hardening following the September 5 deployment audit (tag only; no
release was published, see 0.1.3). See [v0.1.2 notes](docs/releases/v0.1.2.md) and the
[audit remediation record](docs/audit-2026-09-05/REMEDIATION.md).

### Added

- Destination selection (`--dest`) for day-2 commands, an embedded Bunny module, improved
  Bunny rollback identity and optional revision-aware readiness checks, and stronger audit
  history.
- Linux/macOS release gates, real `age` / Caddy fault regressions, pinned build inputs, and
  release provenance attestations.

### Changed

- Ambiguous proxy switches preserve the serving backend; incomplete transitions are journaled
  and release state is committed atomically before cleanup.
- Failed deployments preserve rollback history and configuration; native rollback restores the
  recorded container runtime.
- Caddy application ingress enforces HTTPS (existing listeners are migrated); implicit
  host-wide pruning is removed.
- Captured commands are bounded (600-second default deadline, 16 MiB per output stream) and
  bulk concurrency is capped; per-host deployment locks are owned.
- Behavior changes: unresolved deployment journals require operator reconciliation; HTTP
  redirects are not followed by the HTTP builtins; Caddy application routes need a domain or
  separately configured certificates for HTTPS; relative build contexts remain relative to the
  invoking working directory.

### Security

- Secrets and remote builds use private temporary files; decrypted files are published only
  after authentication succeeds; replay and compensation redaction improved.

[Unreleased]: https://github.com/inou/nrgize-rs/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/inou/nrgize-rs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/inou/nrgize-rs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/inou/nrgize-rs/releases/tag/v0.1.2
