# Changelog

All notable changes to `nrg` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may change behavior).

The full release notes for each tag live in [`docs/releases/`](docs/releases/).

## [Unreleased]

### Added

- `deploy()` option `pre_deploy_version`: a command that prints the database schema version. `nrg` reads it before and after `pre_deploy` (standard output only; the last word of the last line, starting with a letter or digit, containing a digit, from `[A-Za-z0-9._:+-]`, up to 128 characters). If the version it prints did not change, there is nothing to undo; if it did, `{before}` in `pre_deploy_undo` becomes the version before. A failed read before `pre_deploy` stops the deploy; a failed read after it skips the undo and prints the command to run by hand. `nrg rollback --undo-pre-deploy` first checks the database is still at the version the release left, and refuses if it moved. A repeat deploy of the same image that changes nothing keeps the undo saved by the first call, and one that migrates again chains back to the original version. `state_update(values, deleted)` can also delete keys in the same write. Without the key, behaviour is as before.
- `nrg rollback --undo-pre-deploy` (stdlib: `rollback(.., #{ undo_pre_deploy: true })`): after the containers are rolled back, also run the `pre_deploy_undo` that the rolled-back release was deployed with, once on the first host, from that release's image. Off by default. It refuses before touching any host when no undo was saved, does not run if the container rollback fails, and keeps the containers rolled back if the undo fails. `deploy()` now saves `pre_deploy_undo` in the release config when `pre_deploy` ran. Only releases deployed with this version or later can be undone. It also refuses when the target is not the saved previous release, when only some of the hosts the release was deployed to (across deploy calls) are rolled back, and when the target release's migration was already undone (deploy it again first).
- `deploy()` option `pre_deploy_undo`: a command (for example `bin/rails db:rollback`) that undoes `pre_deploy` if the deploy then fails. It runs once on the first host, in a throwaway container of the new image, after the old containers are serving again, and also on Ctrl-C. If any host could not be put back on its old containers, the undo is skipped with a warning that names the host and the command to run by hand. It does not run if `pre_deploy` itself failed, on success, or in `rollback()` (unless you ask with `--undo-pre-deploy`). A failing undo is reported and never hides the original error. Setting it without `pre_deploy` is an error. A dry run prints the command. `deploy()` now runs `pre_deploy` and the Caddy boot inside its fleet transaction; behaviour without `pre_deploy_undo` is unchanged.

### Documentation

- The deploy guide now says what happens to the database when a deploy fails.
- The documentation is rewritten around short guides with runnable examples and diagrams, and the site now renders Mermaid. The roadmap, workflows and deployment-validation pages are removed.
- The documentation site has its own look: a light colour scheme, IBM Plex type, dark code panels with Rhai syntax colours and a copy button, cleaner tables, and diagrams drawn in the site's colours that stay readable on phones. The theme is pinned to just-the-docs v0.12.0.
- Diagrams on the site are laid out with right-angle connectors that no longer cross or kink, questions and failure outcomes are colour-coded, and line labels have a clear background.
- `tests/doc_drift.rs` now checks every `nrg` flag shown in the README and all pages, requires the stdlib reference alone to name every public function, and validates page front matter.

## [0.2.0] - 2026-11-10

One proxy, one way to import the stdlib, one place for secrets, and a dry run that prints
a plan instead of simulating hosts. [v0.2.0 notes](docs/releases/v0.2.0.md).

### Removed (breaking)

- kamal-proxy support: `std/proxy` and the internal `std/proxy_common` modules are gone, and Caddy (`std/caddy`) is the only reverse proxy. Replace `import "std/proxy"` with `import "std/caddy"`, and run `nrg setup` to boot Caddy on hosts that ran kamal-proxy.
- `cfg.proxy` no longer selects a backend: absent or `"caddy"` is accepted, and `"kamal"` (in a script, or in a persisted `<service>.config` or `.prev_config` replayed by `rollback()`) now throws "cfg.proxy names a proxy backend that was removed in 0.2" before anything is mutated. Delete the key; `deploy()` no longer persists it. For a stale value in the persisted config, redeploy the image you want to roll back to, or call `deploy::rollback(hosts, service, #{ proxy: "caddy" })` from a script.
- `nrg setup --proxy`; setup always boots Caddy. Drop the flag; `--proxy-version` now pins the Caddy tag only.
- `deploy.rhai`'s public `px_boot` and `px_deploy` dispatch helpers. Call `std/caddy`'s `proxy_boot` / `proxy_deploy` directly.
- The dry-run simulation layer and its `sim_*` builtins. Each has a replacement; the plan closing note now describes what ran for real and what was assumed:
  - `sim_docker_run`, `sim_docker_stop`, `sim_docker_restart`, `sim_docker_rename`, `sim_docker_remove` and `sim_proxy_switch`: call `ssh_exec(host, cmd)` with the same command (the last argument they took).
  - `sim_container_running(host, name)`: `container_running(host, name)`.
  - `sim_container_healthy(host, name)`: `container_healthy(host, name)`.
  - `sim_image_id(host, tag)`: `container_image_id(host, tag)`.
  - `sim_pick_port(host, base)`: `pick_port(host, base)`, which scans for real in both modes; guard it with `is_dry_run()` and use a placeholder in a plan.
  - `sim_wait_port(host, port)`: `port_open(host, port)`.
  - `sim_http_healthy(url)` and `sim_http_healthy(url, timeout_secs)`: `http_get(url)` and `http_get(url, #{}, timeout_secs)`. The new 3-argument `http_get` takes headers and a timeout in seconds, and in a dry run it probes for real like the other `http_get` forms.
- `nrg vendor` and the `lib/X` stdlib-mirror convention. The stdlib is only available as the embedded `std/X`; to change a function, copy it from `lib/` on GitHub into your script. A project's own `lib/foo.rhai` is now just an ordinary relative import.
- `.env` as a `secret()` source. Move each value with `printf %s "$V" | nrg secrets encrypt KEY`.
- `CMD[...]` secret values and the local shell-out behind them. Fetch the value in CI or your shell instead, e.g. `NRG_SECRET_X=$(op read …) nrg …`.
- `nrg secrets seal` and `nrg secrets unseal`, which only handled whole `.env` blobs. Use `nrg secrets encrypt KEY` per value.
- `NRG_STRICT_TRUST` and the warning about group/world-readable plaintext secrets files: `.energize/secrets*` now holds only ciphertext, and group-writable paths are always trusted.
- The `nrg secrets encrypt VALUE` argv form; the value is read from stdin only.
- The `nrg rollback --file` / `nrg setup --file` module-resolution role: both always use the embedded `std/deploy`, `std/docker` and `std/caddy`, and a `lib/deploy.rhai` next to the project is no longer preferred. `--file` still selects the project for state and audit.

### Changed (breaking)

- `secret()` reads only `$NRG_SECRET_<KEY>` (plaintext) and age-encrypted `ENC[...]` values in `.energize/secrets[.<dest>]`.
- Plaintext values in `.energize/secrets*` are refused with an error naming the key and the fix. Re-encrypt them with `nrg secrets encrypt KEY [--dest D]`.
- An `ENC[...]` token in `$NRG_SECRET_*` is refused; the variable must hold the plaintext. Put the token in `.energize/secrets` instead.
- `nrg secrets encrypt` now takes `KEY`, reads the value from stdin and upserts `KEY=ENC[...]` into `.energize/secrets[.<dest>]` (`--dest D` selects the file). `--print` prints only the token and writes no file, which is the old behaviour.
- `.energize/secrets*` is now meant to be committed. Change `.gitignore` to `.energize/*` plus `!.energize/secrets` and `!.energize/secrets.*`; `nrg secrets init` warns if git ignores the file.
- `lib/*.rhai` and `lib/examples/*.rhai` import siblings as `std/X`, and `nrg init --template` writes the example verbatim; nothing is rewritten on embed. A copied stdlib file that still says `import "lib/X"` must switch to `std/X`.
- `deploy()` without `cfg.proxy` used to run kamal-proxy and now boots and drives Caddy. A `cfg.domain` is accepted by default (it adds a Caddy host match for automatic HTTPS) instead of throwing on the old default backend.
- Dry-run: the stdlib no longer simulates container state. Container mutations are plain `ssh_exec` calls that a dry run plans, and reads (`container_running`, `container_image_id`, `container_healthy`, `port_open`, `pick_port`, and `http_get` with a timeout) are new builtins that look at the real host. A read after a planned mutation returns the state from before the run. A dry run shows the new container's host port as `<auto>`, and `wait_healthy`, `wait_port` and `wait_container_healthy` print `[dry-run] assuming ...` and succeed without probing. `accessory_run` skips its exited-immediately check in a dry run, and `accessory_upgrade` plans the start of the new image instead of reporting the accessory as already running. A probe that cannot run is noted in the plan as `[probe failed — assuming <x> absent]`; a live run still throws.
- Dry-run no longer keeps a model of the hosts, so a dry run now differs from before in these ways:
  - A script of your own that reads container state after a planned write no longer sees that write. `container_running` on a container a planned `docker run` would start is `false`. Guard such reads with `is_dry_run()`.
  - The new container's host port in a plan is the placeholder `<auto>`, not a deterministic number such as `13000`.
  - `docker_image_id` of an image that is not on the host is `""` in a dry run; it was the placeholder `"<tag>"`.
  - Waits no longer add rows to the PLAN block: the `check` rows for picking a port, waiting for a port, waiting for a container to be healthy and `[assumed healthy] GET` are gone, and the `[dry-run] assuming ...` notes go to stdout instead.
  - A dry-run `wait_port` no longer fails for a port that was never picked; it passes.
  - The plan caveat line at the end of the plan is reworded.
- The trace line and diagnostics-journal name for container mutations is `ssh_exec <host>` instead of `sim_docker_run <host>` (and the like). The commands, their order and error handling are unchanged.

### Added

- Typed container reads that look at the real host, also in a dry run: `container_running(host, name)`, `container_healthy(host, name)`, `container_image_id(host, tag)`, `port_open(host, port)` and `pick_port(host, base)`. In a dry run a read that cannot run is noted in the plan and answered as absent.
- `http_get(url, headers, timeout_secs)`, a 3-argument form that takes headers and a timeout in seconds and probes for real in a dry run.
- `synthetic_http_response(status)`, which builds the `HttpResponse` (the given status, empty body) that a dry run returns for a wait it assumes passes.

### Documentation

- Docs present `nrg run <fn>` as the way to run a script; `nrg exec` remains as "evaluate a file's top level; useful for one-off scripts".

### Removed

- The development-history documents (`docs/robustness-review.md`, `docs/archive/`, `docs/superpowers/`, `docs/audit-2026-09-05/`) are gone, and `docs/roadmap.md` now lists only open items; use git history and the release notes under `docs/releases/` for the past.

## [0.1.5] - 2026-11-10

Hardening, static Linux binaries and documentation that matches the code. See
[v0.1.5 notes](docs/releases/v0.1.5.md).

### Added

- Statically linked `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` release
  archives. `scripts/install.sh` prefers them on Linux; the glibc archives keep their names.
- `--dest` is one global flag, accepted before or after the subcommand. Repeating it with
  different values is now an error instead of the last one silently winning.
- `tests/doc_drift.rs` fails the build when a builtin, public stdlib function, CLI flag or
  relative doc link falls out of sync with the docs.

### Changed

- The `proxy_boot` pull / remove / run scaffolding and `proxy_stop` body that `std/proxy`
  (kamal-proxy) and `std/caddy` each carried a copy of now live in a new internal module,
  `std/proxy_common`, which both import (issue #23). Behaviour, commands, error messages and
  dry-run plans are unchanged; `nrg vendor` now also writes `lib/proxy_common.rhai`.

### Fixed

- `wait_healthy` and `wait_healthy_on_host` now require `timeout` to be a positive integer or
  a positive finite float (e.g. `2.5`, which remote `curl --max-time` accepts) and reject
  strings and everything else before any command is built; the value was spliced unquoted into
  the remote `curl --max-time`, and a string slipped past the `< 1` check. Caddy
  `proxy_maintenance` requires `status_code` to be an integer 100-599 (issue #10). There is
  still no generic quoting-aware command builder; callers continue to `sh_quote` by hand.
- State writes (`state_set`, `state_update`, `state_del`, purge) now serialize their reload,
  mutate, write sequence under a short-lived lock on `.energize/state.write.lock`, so
  concurrent nested or backgrounded `nrg` processes no longer lose each other's keys
  (issue #28). It is separate from the run lock, so nested runs cannot deadlock on it.
- `nrg secrets init` sets `.nrg-key.pub` to `0644` explicitly (it followed the umask, and a
  `0666` public key was then refused by nrg's own trust check) and fails if it cannot set the
  private key to `0600` (issue #14).

### Security

- `secret()` warns once per file when a plaintext secrets file (`.energize/secrets*`, `.env`)
  that yields a value is readable by its group or others; `NRG_STRICT_TRUST=1` refuses it.
  `nrg secrets init` now asks `git check-ignore`, so parent and global ignore rules count when
  deciding whether to warn about `.nrg-key`. The decision not to support passphrase-protected
  age identities is recorded in `docs/safety.md` (issue #14).

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
  archives exist from v0.1.5 onwards.
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
release was published, see 0.1.3). See [v0.1.2 notes](docs/releases/v0.1.2.md).

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

[Unreleased]: https://github.com/inou/nrgize-rs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/inou/nrgize-rs/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/inou/nrgize-rs/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/inou/nrgize-rs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/inou/nrgize-rs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/inou/nrgize-rs/releases/tag/v0.1.2
