# Contributing to Energize (`nrg`)

Thanks for considering a contribution. This project favors small, well-tested slices over
large speculative changes — see [`docs/safety.md`](docs/safety.md#known-limits) for the known limits, and
[`docs/safety.md`](docs/safety.md) for the guarantees and limits to preserve when
touching `lib/deploy.rhai` or anything in `src/engine/`.

## Before you start

Read [`docs/architecture.md`](docs/architecture.md) for engine internals (the `RunCtx`,
builtin registration, the dry-run recording, the transaction stack) and
[`docs/authoring.md`](docs/authoring.md) for the Rhai-specific gotchas (`trim()` mutates in
place, `state_get` of an absent key is `()` not `""`, imports are per-file, etc.) that show up
repeatedly in both the stdlib and its tests.

## The gate

Every change should pass, locally, before you open a PR:

```bash
cargo build --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
CI=true cargo test --all-targets --locked --no-fail-fast
python3 tests/audit_regressions.py "$PWD"
python3 tests/audit_external_regressions.py "$PWD" "$(dirname "$(command -v age)")"
cargo audit
```

`cargo audit` needs `cargo install cargo-audit --locked`; CI runs it after the tests.

`CI=true` matters — some tests behave differently (fail loud instead of skipping) when a
required external tool like Docker or `age` isn't available, to keep CI from silently losing
coverage. If you're touching `scripts/install.sh`, also run `shellcheck -s sh scripts/install.sh`.

## Tests

- New behavior needs a new test — this codebase leans heavily on `assert_cmd` integration
  tests (see `tests/*.rs`) that actually invoke the compiled `nrg` binary end-to-end, rather
  than only unit-testing internals.
- `tests/doc_drift.rs` fails when docs fall behind the code: every new Rhai builtin, public
  stdlib fn or embedded module, and CLI subcommand or `--flag` needs a matching entry in
  `docs/builtins.md`, the stdlib docs or `docs/cli.md` in the same change, and relative links and
  anchors in `docs/*.md` must resolve.
- Dry-run mode with a `FakeRunner` lets `lib/*.rhai` logic be tested without a real Docker
  daemon or SSH target — prefer testing through it over mocking at a lower level.
- **Mutation-verify anything that changes conditional logic**: temporarily break the new
  guard/check (comment it out, invert the condition, etc.), confirm the relevant test fails
  *for the right reason*, then restore the original code and confirm it passes again. This
  catches tests that would pass even if the fix were reverted — a real, still-common failure
  mode.

## Style

- No unnecessary abstractions, comments that restate the code, or speculative
  generality: three similar lines beat a premature helper, and comments explain the
  non-obvious *why*, not the *what*.
- Keep `docs/*.md` in sync with behavior changes — several docs pages (`cli.md`, `deploy.md`,
  `stdlib.md`) are treated as living references, not one-time snapshots.

## Security-sensitive areas

`scripts/install.sh` (a `curl | sh` installer that prefers the static musl archives and falls
back to gnu only on HTTP 404) and `.github/workflows/release.yml` (a CI/CD release pipeline
with `contents: write`, which also builds the musl archives and opens the Homebrew formula bump
PR via `scripts/bump-formula.py`) deserve extra scrutiny — any change to secret
handling, checksum verification, command construction, or the release matrix should be
reviewed with that in mind.
