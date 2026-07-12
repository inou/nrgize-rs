# Contributing to Energize (`nrg`)

Thanks for considering a contribution. This project favors small, well-tested slices over
large speculative changes — see [`docs/roadmap.md`](docs/roadmap.md) for what's already
tracked as a known gap, and [`docs/robustness-review.md`](docs/robustness-review.md) for the
history of hardening passes this codebase has already been through (useful context before
touching `lib/deploy.rhai` or anything in `src/engine/`).

## Before you start

Read [`docs/architecture.md`](docs/architecture.md) for engine internals (the `RunCtx`,
builtin registration, the dry-run simulation overlay, the transaction stack) and
[`docs/authoring.md`](docs/authoring.md) for the Rhai-specific gotchas (`trim()` mutates in
place, `state_get` of an absent key is `()` not `""`, imports are per-file, etc.) that show up
repeatedly in both the stdlib and its tests.

## The gate

Every change should pass, locally, before you open a PR:

```bash
cargo build --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
CI=true cargo test --all-targets --locked
```

`CI=true` matters — some tests behave differently (fail loud instead of skipping) when a
required external tool like Docker or `age` isn't available, to keep CI from silently losing
coverage. If you're touching `scripts/install.sh`, also run `shellcheck -s sh scripts/install.sh`.

## Tests

- New behavior needs a new test — this codebase leans heavily on `assert_cmd` integration
  tests (see `tests/*.rs`) that actually invoke the compiled `nrg` binary end-to-end, rather
  than only unit-testing internals.
- The dry-run simulation (`src/engine/sim.rs`) exists specifically so `lib/*.rhai` logic can
  be tested without a real Docker daemon or SSH target — prefer testing through it over
  mocking at a lower level.
- **Mutation-verify anything that changes conditional logic**: temporarily break the new
  guard/check (comment it out, invert the condition, etc.), confirm the relevant test fails
  *for the right reason*, then restore the original code and confirm it passes again. This
  catches tests that would pass even if the fix were reverted — a real, still-common failure
  mode.

## Style

- No unnecessary abstractions, comments that restate the code, or speculative
  generality — see the top-level engineering norms this session followed throughout: three
  similar lines beat a premature helper, and comments explain the non-obvious *why*, not the
  *what*.
- Keep `docs/*.md` in sync with behavior changes — several docs pages (`cli.md`, `deploy.md`,
  `stdlib.md`, `roadmap.md`) are treated as living references, not one-time snapshots.

## Security-sensitive areas

`scripts/install.sh` (a `curl | sh` installer) and `.github/workflows/release.yml` (a CI/CD
release pipeline with `contents: write`) deserve extra scrutiny — any change to secret
handling, checksum verification, command construction, or the release matrix should be
reviewed with that in mind.
