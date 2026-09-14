---
title: Recipe contracts and rehearsal
nav_order: 19
---

# Recipe contracts and a local failure playground

`nrg rehearse` tests explicitly declared deployment behavior using real commands
in a fresh temporary directory. A contract names actions, expected failures, and
observable postconditions. It can exercise any framework, runtime, supervisor,
or artifact format; none is required by the runner.

The command defaults to **declaration validation**, without executing shell
commands. Live execution requires `--execute`. Fault scenarios additionally
require `--faults`. This is separate from a deployment's `Energize.rhai`.

## Try the working example

From a checkout of this repository, with Rust and Python 3.9+ available:

```sh
cargo build --locked
target/debug/nrg rehearse examples/rehearsal/Rehearsal.rhai
target/debug/nrg rehearse examples/rehearsal/Rehearsal.rhai --execute --faults
target/debug/nrg rehearse examples/rehearsal/Rehearsal.rhai --execute --faults --json > rehearsal.json
```

The [contract](https://github.com/inou/nrgize-rs/blob/main/examples/rehearsal/Rehearsal.rhai) drives a small
[reference recipe](https://github.com/inou/nrgize-rs/blob/main/examples/rehearsal/service.py). It starts an actual Python
HTTP service on a kernel-assigned loopback port and checks:

- First installation, repeat installation, and restart still serve `v1`.
- A candidate returning HTTP 503 is rejected with exit 42 and `v1` is restored.
- A writer is interrupted **after** its partial file appears; its cleanup removes
  that file, the command returns exit 43, and the original service still works.
- The owned service is killed, observed to stop responding, restarted, and checked.

The example stops its service during cleanup. nrg then removes the entire
workspace. These are actual process, HTTP, signal, and filesystem checks. They
do not certify any framework, production supervisor, SSH server, or deployment
recipe other than the reference implementation exercised here.

The loopback server avoids reverse hostname lookups. Its regression test rejects
such lookups explicitly, keeping host DNS configuration out of the local fixture's
startup requirements.

## Write a contract

Save this framework-independent example as `Rehearsal.rhai`:

```rhai
import "std/contracts" as c;

c::service(#{
    name: "artifact recipe",
    deploy: c::step("install artifact", "printf v1 > current"),
    restart: c::step("reload artifact", "test -f current"),
    checks: [c::step("version remains v1", "test \"$(cat current)\" = v1")],
    cleanup: [c::step("remove artifact", "rm -f current")],
    faults: [c::rejects("reject candidate", "exit 42", 42, [
        c::step("original remains available", "test \"$(cat current)\" = v1")
    ])]
})
```

This small example verifies files and orchestration only. Its `exit 42` is a
declared synthetic fault, not evidence of a real upload failure or rollback.
Use the HTTP example above for real failure injection, and replace the commands
with your own disposable recipe before claiming application compatibility.

```sh
nrg rehearse                    # default file; declarations only
nrg rehearse --dry-run --faults  # include fault steps in the unverified plan
nrg rehearse --execute           # first install, repeat, restart; faults skipped
nrg rehearse --execute --faults  # run the complete declared contract
```

`service(cfg)` builds three scenarios in order: **first install**, **repeat
install**, and **restart**, then appends `cfg.faults`. The repeat uses the same
deployment command, and every scenario runs the supplied checks. Optional `setup`
runs once first; mandatory `cleanup` runs last. The helper does not invent a
health endpoint or determine whether deployment is idempotent.

For custom lifecycles, return a map directly:

```rhai
import "std/contracts" as c;

#{
    name: "custom lifecycle",
    setup: [],
    scenarios: [c::scenario("create", [
        c::step("create file", "printf ready > result")
    ], [c::step("verify bytes", "test \"$(cat result)\" = ready")])],
    cleanup: [c::step("remove file", "rm -f result")]
}
```

All selected scenarios share one workspace and run in declaration order. A new
invocation gets a new workspace. Steps use a fresh `/bin/sh -c` process; shell
variables and `cd` do not persist between steps. Files do persist. nrg preserves
ordinary shell exit behavior; use `set -eu` yourself where appropriate.

## Declaration reference

| Helper | Declaration |
| --- | --- |
| `step(name, command)` | Action or check expecting exit 0 |
| `scenario(name, steps, checks)` | Ordinary scenario with actions and postconditions |
| `rejects(name, command, exit_code, checks)` | Fault expecting one exact nonzero exit, followed by successful checks |
| `fault(name, steps, checks)` | Fault with multiple explicit actions, such as kill then restart |
| `service(cfg)` | Common first/repeat/restart lifecycle and optional faults |

`service` accepts `name`, optional `setup`/`env`/`faults`, and required `deploy`,
`restart`, `checks`, `cleanup`. Unknown keys are errors.

The raw contract accepts `name`, optional `setup` and `env`, mandatory `scenarios`
and `cleanup`. A scenario has `name`, `steps`, `checks`, and optional `fault: true`.
Every scenario must have at least one action and one postcondition. Scenario names
are unique; `setup` and `cleanup` are reserved. Names must be printable and at most
256 bytes. Contracts are limited to 64 scenarios and 256 total steps.

A step is a map with `name`, `run`, optional `expect_exit` (default 0),
`timeout_secs` (default 60, range 1–3600), and `stdin` (default empty). For example:

```rhai
#{ name: "bounded build", run: "./build.sh", timeout_secs: 300, stdin: "" }
```

Only fault **actions** may expect a nonzero exit, in the range 1–125. Setup,
cleanup, and postconditions must expect 0. Missing commands (127), signal exits,
runner errors, and timeouts cannot accidentally satisfy an expected rejection.
A matching fault exit alone does not pass the contract: its checks must pass too.
An execution request with no selected scenarios is rejected before creating a
workspace. Faults omitted without `--faults` remain visibly skipped.

## Adapt a framework recipe

Use the same build defaults as the real recipe, then supply test-owned deployment,
restart, request, and cleanup adapters. Both `std/contracts` and
`std/release_recipes` are available in contract declarations. For example, this
**adaptation sketch requires you to provide its fixture and adapter scripts**:

```rhai
import "std/contracts" as c;
import "std/release_recipes" as recipes;

let recipe = recipes::phoenix(#{}); // Rails, Django, Next.js and Laravel work similarly.
c::service(#{
    name: "application release contract",
    env: recipe.env,
    setup: [
        c::step("copy test application", "cp -R \"$NRG_SOURCE/fixtures/app/.\" ."),
        c::step("build application", recipe.build)
    ],
    deploy: c::step("deploy locally", "sh \"$NRG_SOURCE/rehearsal/deploy.sh\""),
    restart: c::step("restart locally", "sh \"$NRG_SOURCE/rehearsal/restart.sh\""),
    checks: [c::step("request and version", "sh \"$NRG_SOURCE/rehearsal/check.sh\"")],
    cleanup: [c::step("stop owned resources", "sh \"$NRG_SOURCE/rehearsal/cleanup.sh\"")]
})
```

Provide actual runtime executables on `PATH`; an isolated home may make an
unconfigured runtime-manager shim unusable. Put capability/version probes in
`setup` when relevant. nrg installs no runtimes and requires neither age nor a
container engine. Framework test adapters should use test data and own their
processes, ports, databases, and other resources explicitly.

## Workspace, environment, and cleanup

Every command starts in the temporary workspace. Its environment contains only
the inherited `PATH`, contract `env`, explicitly passed `--env NAME` values, and
these runner-controlled paths:

- `NRG_WORKSPACE`: the fresh workspace; managed files should live beneath it.
- `NRG_SOURCE`: the directory containing the contract. Input files are not copied
  automatically; copy selected fixture files into the workspace before building.
- `NRG_BIN`: the absolute path of this nrg executable.
- `HOME`, `TMPDIR`, and XDG config/cache/data/state paths: directories inside the
  workspace. These and the `NRG_`/`XDG_` namespaces cannot be overridden.

Shell startup variables such as `ENV`/`BASH_ENV` are also rejected. `--env NAME`
overrides a contract environment value and requires the variable during live
execution. It never accepts a `NAME=value` argument. nrg does not discover project
secrets or inherit SSH-agent/cloud credentials automatically.

**This is not an OS security sandbox.** Trusted shell commands still have the
user's filesystem and network permissions and can escape the workspace or contact
remote systems. Use commands explicitly written for disposable resources. This
version has no VM/container isolation backend and accepts no deployment `--dest`.

On an unexpected action or check result, later work is skipped. All cleanup steps
are still attempted, even if an earlier cleanup fails. A cleanup failure fails
the run. The first SIGINT/SIGTERM cancels the active process group and permits
bounded cleanup; the second forces exit. Cleanup must stop any background services
or external resources it created. Removal of temporary files cannot do that for
it. SIGKILL, machine failure, or detached descendants can prevent full cleanup.

## Reports and CI

Plain output streams stdout/stderr concurrently with bounded diagnostic tails.
`--json` suppresses command streaming and emits one report on stdout; step progress
still goes to stderr. Save JSON with shell redirection for a CI artifact. No
deployment audit, journal, lock, or state file is created by the rehearsal runner.

Schema version 1 reports overall status, workspace/removal status, and each step's
scenario, name, local host, operation, fault flag, expected/actual exit, status,
duration, and bounded error excerpt. Expected nonzero failures retain evidence too.
Unexecuted steps have a null actual exit and duration. Plans are explicitly
execution-unverified; a skipped fault contributes no compatibility evidence.

Command bodies, stdin and environment values are omitted from reports and plans.
Explicit environment and stdin values are registered for redaction, including
secrets split across output chunks. Avoid secret literals in command strings and
avoid passing credentials in subprocess arguments. Redaction covers registered
raw/JSON-escaped/shell-quoted forms, not unknown values or arbitrary transformations.

Exit codes: **0** means declarations validated or selected live checks passed;
**1** means execution/cleanup failed; **2** means invalid arguments/declarations;
**130** means interrupted. Check the report status to distinguish declaration
validation from execution. This command is additive: existing `exec`, `run`,
`doctor`, deployment scripts and framework recipes retain their behavior. It
requires a build containing this feature; older tagged binaries lack `rehearse`.
