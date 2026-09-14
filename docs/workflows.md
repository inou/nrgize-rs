---
title: General and framework workflows
nav_order: 18
---

# General deployment primitives, optional framework recipes

`nrg` runs Rhai orchestration for any toolchain. SSH, files, checked commands,
preflight checks, transactions and versioned releases do not require Elixir,
containers or age. Framework recipes are optional configuration helpers.

## Checked commands with explicit options

```rhai
local_step("Build artifact", "./scripts/build", #{
    cwd: "services/api",
    env: #{ BUILD_TOKEN: secret("BUILD_TOKEN").reveal() },
    stdin: "",
    timeout_secs: 300,
    stream: true
});

ssh_step("deploy@web1", "Fetch artifact", "./fetch-artifact", #{
    cwd: "/srv/staging",
    timeout_secs: 120,
    retries: 2,
    retry_delay_ms: 1000,
    idempotent: true
});
```

The existing two-argument `local_step` and three-argument `ssh_step` remain valid.
Both throw on failure; legacy `local_exec`/`ssh_exec` still return an `ExecResult`
whose `.ok` the script must inspect. Options reject unknown keys, invalid environment
names, zero timeouts and excessive retries. `retries` means *additional* attempts
(maximum 10), and requires an explicit `idempotent: true` declaration. nrg does not
determine whether repeating a shell command is safe. Delay defaults to 1000 ms and
is bounded at 60000 ms. The default timeout remains `NRG_COMMAND_TIMEOUT_SECS` or
600 seconds. Timeout or interruption does not prove a remote command stopped.

Environment values and stdin are excluded from nrg transport arguments, plans and
run metadata. User commands must also avoid copying those values into child-process
arguments. nrg registers their nonempty values for redaction before streaming;
short values can therefore obscure ordinary matching text too. Local commands get
environment variables and piped stdin directly. Remote commands with execution
options receive a framed private script and input file through SSH stdin, using `sh`, `mktemp`, `dd`, `cat`,
`wc` and `rm`. Files start private and are removed on normal completion and handled
signals. SIGKILL, network loss or host failure can prevent cleanup. Original
overloads retain their SSH invocation. Configured remote steps run a private script (so shell `$0` identifies that script). Command exit status and shell semantics
are preserved; nrg does not add `set -e` to user command bodies. Environment variables are inherited by child
processes; they are not a substitute for a host's process-access controls.

Both output pipes are drained concurrently, streamed promptly without requiring a
newline, and redacted across chunk boundaries. Each named step retains an 8 KiB
tail per stream; audit failures retain a smaller bounded excerpt. Redaction covers
registered raw, JSON-escaped and shell-quoted values, not arbitrary encoding or
unregistered credentials. Avoid embedding secret literals into shell commands.

SIGINT/SIGTERM cancels the active local process group and fails the run, including
when it was the final statement. Transaction compensation can then run. Remote
services or detached descendants may outlive the local SSH process; inspect the
host before retrying an interrupted operation.

## Versioned releases without a container requirement

Start with `nrg init --template release`, or write:

```rhai
import "std/release" as release;

fn config() {
    #{
        prepare: "cp -R /srv/staged/api/. .",
        build: "./build.sh",
        // Set migrate explicitly if this release requires a database change.
        activate: "sudo -n systemctl restart api",
        health: "curl --fail --silent --show-error http://127.0.0.1:8080/health",
        deactivate: "sudo -n systemctl stop api",
        timeout_secs: 300
    }
}
fn deploy(version) {
    release::deploy("deploy@web1", "/srv/api", version, config());
}
fn rollback(version) {
    release::rollback("deploy@web1", "/srv/api", version, config());
}
```

`nrg run deploy v42` creates `/srv/api/releases/v42` exclusively, runs prepare,
optional build and optional migrate there, atomically replaces `/srv/api/current`,
then runs activate and health. Activation and health run from `/srv/api`. All
hooks receive `NRG_RELEASE_DIR`, `NRG_CURRENT_LINK` and `NRG_PREVIOUS_RELEASE` in
their environment, plus optional `cfg.env`. Configure the supervisor to use the
`current` link. The helper does not install or configure that supervisor.

If activation or health fails, compensation restores the previous link and runs
activate and health for that release. A failed first release has its link removed;
optional `deactivate` can stop its service. Failed build/migration leaves `current`
untouched. Database migrations and external hook side effects are not undone.
Rollback compensation failures are reported while preserving the original error.

`nrg run rollback v41` explicitly selects an existing directory and uses the same
activation/health/compensation flow. It does not rebuild or migrate. Release
directories are retained for inspection; reusing a version for a new deployment
fails instead of overwriting it. There is no automatic pruning, crash recovery,
shell-command resume, fleet atomicity or zero-downtime guarantee in this helper.

The root must be an application-owned absolute directory with trusted parents on
a POSIX host. A per-root remote lock serializes this lifecycle. Existing `current`
must be a symlink to an absolute release path under the root. Switching uses a
sibling temporary symlink and explicit GNU/BSD `mv` no-dereference options. Test
the intended host's tools and filesystem; a dry-run plan cannot establish atomic
rename support there. Do not nest this lifecycle inside another transaction.

The existing container deployment modules, framework `nrg init --template`
starters, and container-oriented `nrg rollback` command retain their behavior.
For directory releases, use the explicit Rhai rollback wrapper shown above.

## Framework defaults you can replace

`std/release_recipes` exports `rails(cfg)`, `django(cfg)`, `nextjs(cfg)`,
`phoenix(cfg)` and `laravel(cfg)`. These return maps for `release::deploy`.

| Recipe | Default build work | Environment default |
| --- | --- | --- |
| Rails | Bundle install, asset precompilation | `RAILS_ENV=production` |
| Django | Per-release venv, requirements install, collectstatic | None; supply settings |
| Next.js | npm ci including build dependencies, npm run build | `NODE_ENV=production` |
| Phoenix | Production dependencies, compile, assets.deploy, release | `MIX_ENV=prod` |
| Laravel | Composer install and config/route/view caches | `APP_ENV=production` |

These assume conventional project tasks and installed runtime/build tools. Every
command can be replaced. `env` maps merge by key; other caller values replace
defaults. Prepare, activate and health are required application choices. Migration
is opt-in: for example `bundle exec rails db:migrate`,
`.venv/bin/python manage.py migrate --noinput`, or `php artisan migrate --force`.
Phoenix release migrations require an application-provided release task.
Next.js has no universal database migration command. Shared files, uploads,
secrets, dependency caches and database services remain explicit app configuration.

```rhai
import "std/release" as release;
import "std/release_recipes" as recipes;
import "std/mise" as mise;

fn deploy(version) {
    let host = "deploy@web1";
    let tools = #{ erlang: "28.5.0.6", elixir: "1.20.4-otp-28" };
    let toolchain = "/srv/myapp/toolchain";
    // mise and its OS build dependencies must already exist on this host.
    mise::provision(host, toolchain, tools);
    let cfg = recipes::phoenix(#{
        prepare: "cp -R /srv/staged/myapp/. .",
        activate: "sudo -n systemctl restart myapp",
        health: "curl --fail --silent --show-error http://127.0.0.1:4000/health",
        deactivate: "sudo -n systemctl stop myapp"
    });
    cfg.build = mise::command(toolchain, tools,
        "cd \"$NRG_RELEASE_DIR\" && " + cfg.build);
    release::deploy(host, "/srv/myapp", version, cfg);
}
```

This reuses the tested Erlang-before-Elixir app-scoped recipe. Other frameworks can
use another runtime manager, preinstalled binaries, or prebuilt artifacts.

## Run history and automation

Live `nrg exec` and `nrg run` write `.energize/runs.jsonl` alongside the existing
audit log. Each invocation has a run ID; step start, step finish and run finish
events are appended and synced as work proceeds. Start events have **no exit
code**. Named steps, ordinary exec effects, file transfers and preflight checks
record metadata and bounded redacted failures. This is a journal of instrumented
operations, not a trace of every shell statement. Write failures produce warnings
and do not change the deployment outcome. Existing logs remain readable.

```sh
nrg audit                     # failed step details alongside the final outcome
nrg audit web1 --verbose      # filter also matches step names, hosts and failures
nrg audit --json              # structured final audit entries
nrg audit --run RUN_ID --json # event timeline
nrg audit --incomplete        # started runs without a completion event
nrg audit --limit 0           # no display limit
nrg status --check --json     # container status suitable for automation
```

An incomplete run might still be running; inspect it before retrying. nrg never
resumes arbitrary shell operations automatically. Malformed journal lines are
reported and skipped; a subsequent run separates a torn trailing line from its
new events. The journal remains after a process crash but cannot establish the
remote outcome.

`status --check` fails when there are no recorded hosts, a probe fails, a container
is stopped/absent, or a running container reports unhealthy/starting. A running
container without a healthcheck is accepted as running; JSON preserves that health
is unknown. Missing runtime binaries and malformed inspect output are probe
failures, not evidence of an absent container. The default status command retains
its informational exit behavior. `--offline` conflicts with `--check`. This status
command still reads the container deployment state; use the chosen health hook
for directory releases.

Dry runs create no journal, deployment locks or state files. Arbitrary shell
operations remain **planned; execution-unverified**. Syntax validation, declared
read-only capabilities and explicitly requested temporary-write preflight checks
remain separate; see [preflight reference](builtins.md#preflightchecks-allow_temporary).
