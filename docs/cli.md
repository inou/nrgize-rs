---
title: CLI Reference
nav_order: 3
---

# `nrg` CLI reference

`nrg` (Energize) is a Rhai-powered SSH orchestration runner. You write deploy
logic in a Rhai orchestration file (`Energize.rhai` by default), and `nrg`
evaluates it — either top-to-bottom (`nrg exec`) or by calling a named function
(`nrg run <fn>`).

```
nrg <command> [args]
```

| Command | Purpose |
| --- | --- |
| [`nrg exec [file] [--dry-run]`](#nrg-exec) | Evaluate an orchestration file top-to-bottom |
| [`nrg run <fn> [args...] [--file <path>] [--dry-run]`](#nrg-run) | Call a function defined in the orchestration file |
| [`nrg tasks [--file <path>]`](#nrg-tasks) | List the functions defined in the orchestration file |
| [`nrg init`](#nrg-init) | Scaffold a starter `Energize.rhai` |
| [`nrg doctor [--file <path>] [--host h]...`](#nrg-doctor) | Check the file compiles, required tools are installed, and hosts are reachable |
| [`nrg ssh <host>`](#nrg-ssh) | Open an interactive SSH session, resolving `~/.ssh/config` aliases |
| [`nrg secrets <subcommand>`](#nrg-secrets) | Manage encrypted secrets via [`age`](https://github.com/FiloSottile/age) |
| [`nrg status [service] [--offline]`](#nrg-status) | Show the deployed version/image and per-host container state |
| [`nrg audit [filter] [--limit N]`](#nrg-audit) | Show the history of past `nrg exec`/`nrg run` invocations |
| [`nrg logs <service> [--host h] [--follow] [--lines n]`](#nrg-logs) | Tail a service's container logs across its deployed hosts |
| [`nrg app exec <service> [--host h] [-i] [cmd...]`](#nrg-app-exec) | Run a command inside a service's live container |

`nrg --version` and `nrg --help` (and `nrg <command> --help`) are available
on every command (provided by clap).

Every command returns `0` on success and a non-zero code on failure
(typically `1`). See [Exit codes](#exit-codes-and-the-failure-contract).

---

## The orchestration file

By default `nrg exec`, `nrg run`, `nrg tasks`, and `nrg doctor` look for the
orchestration file in the current directory, trying in order:

1. `Energize.rhai`
2. `energize.rhai`

If neither exists (and you didn't pass a file / `--file`), the command errors.
The file is a Rhai module. A minimal example:

```rhai
// Energize.rhai
import "lib/docker" as docker;   // imports MUST be at the top level

let HOSTS = ["deploy@web1.example.com"];

fn deploy() {
    for host in HOSTS {
        let r = ssh_exec(host, "cd /srv/app && git pull origin main");
        if !r.ok { throw "deploy failed on " + host + ": " + r.stderr; }
    }
    print("Deployed to all hosts.");
}
```

`import` is resolved relative to the directory of the file being run, so
`import "lib/docker" as docker;` loads `<file-dir>/lib/docker.rhai`. Imports
must appear at the **top level** of the module (Rhai does not allow `import`
inside a function body).

---

## `nrg exec`

Evaluate a Rhai orchestration module top-to-bottom. Builtins (`ssh_exec`,
`http_get`, `state_set`, …) take effect as evaluation reaches them.

```
nrg exec [file] [--dry-run] [--lock-timeout <seconds>]
```

| Argument / flag | Meaning |
| --- | --- |
| `[file]` | Path to the `.rhai` file. Defaults to `Energize.rhai` / `energize.rhai`. |
| `--dry-run` | Show the plan of side effects without executing them. |
| `--lock-timeout <seconds>` | Give up waiting for the state lock after this many seconds instead of blocking forever (see [State and locking](#state-and-locking)). `0` means "fail immediately if the lock isn't already free" rather than "wait forever" — pass no flag at all for the indefinite-wait default. |

```bash
nrg exec                          # run ./Energize.rhai top-to-bottom
nrg exec deploy.rhai               # run a specific file
nrg exec --dry-run                 # preview what would happen
nrg exec --lock-timeout 60         # give up after 60s if another run holds the lock
```

A live run takes an advisory lock and writes state to disk (see
[State and locking](#state-and-locking)). `--dry-run` takes **no lock** and
writes **no state** — it uses an in-memory overlay and prints a rendered plan
to stdout after evaluation. See [Dry-run behavior](#dry-run-behavior) for
exactly what each builtin does in dry-run.

---

## `nrg run`

Call a single function defined in the orchestration file.

```
nrg run <fn> [args...] [--file <path>] [--dry-run] [--lock-timeout <seconds>]
```

| Argument / flag | Meaning |
| --- | --- |
| `<fn>` | Name of the function to call in the orchestration file (required). |
| `[args...]` | Positional arguments passed to the function. **All args are passed as Rhai strings.** |
| `--file <path>` | Path to the `.rhai` file. Defaults to `Energize.rhai` / `energize.rhai`. |
| `--dry-run` | Show the plan of side effects without executing them. |
| `--lock-timeout <seconds>` | Give up waiting for the state lock after this many seconds instead of blocking forever (see [State and locking](#state-and-locking)). `0` means "fail immediately if the lock isn't already free" rather than "wait forever" — pass no flag at all for the indefinite-wait default. |

```bash
nrg run deploy                    # call deploy()
nrg run rollback web1 v8          # call rollback("web1", "v8")
nrg run deploy --dry-run          # preview deploy()
nrg run deploy --file ops/Deploy.rhai
nrg run deploy --lock-timeout 60  # give up after 60s if another run holds the lock
```

How it works: the file's top level is evaluated first (so top-level
`import`s, config `let`s, and any setup run), then the named function is
invoked with the CLI args.

### Arguments are always strings

Every positional argument reaches the function as a **string**, regardless of
how it looks. `nrg run scale 3` calls `scale("3")`, not `scale(3)`. If your
function needs a number, parse it from the string inside Rhai.

### A function argument that starts with `-` needs a `--` separator

Flags (`--dry-run`, `--file`) may appear anywhere on the line. Because of this,
a **function argument** that itself begins with `-` would be mistaken for a
flag. Put it after a `--` separator:

```bash
nrg run set_flag -- --verbose     # calls set_flag("--verbose")
nrg run note -- -n/a              # calls note("-n/a")
```

Without the `--`, `nrg run set_flag --verbose` fails because `--verbose`
isn't a known `nrg` flag.

### Missing functions are refused before anything runs

`nrg run <typo>` does **not** silently run the file's top level and then fail.
The function is checked at compile time; if it isn't defined, you get an error
and nothing executes:

```
Error: no function `deplooy` defined in Energize.rhai. `nrg run <fn>` calls a
function; use `nrg exec Energize.rhai` to run a top-level script.
```

This means `nrg run` is for **calling functions**. If your file's logic lives
at the top level (no `fn`), use `nrg exec` instead.

---

## `nrg tasks`

List the functions defined in the orchestration file. Each one is a callable
entry point for `nrg run <fn>`.

```
nrg tasks [--file <path>]
```

| Flag | Meaning |
| --- | --- |
| `--file <path>` | Path to the `.rhai` file. Defaults to `Energize.rhai` / `energize.rhai`. |

```bash
nrg tasks
```

```
Functions:
  deploy
  rollback (2 args)
  uptime
```

Functions are listed sorted by name, with their parameter count shown when
non-zero (`(1 arg)` / `(2 args)`). `nrg tasks` only **parses** the file — it
does not run the top level or resolve `import`s, so it lists only functions
defined directly in that file (not ones pulled in from imported `lib/` modules).

---

## `nrg init`

Scaffold a starter `Energize.rhai` in the current directory.

```
nrg init
```

`nrg init` takes no arguments. It writes a template `Energize.rhai` with a
`deploy()` and `uptime()` example. If `Energize.rhai` already exists, it
**refuses** and exits non-zero rather than overwriting:

```
Error: Energize.rhai already exists.
```

---

## `nrg doctor`

Sanity-check your setup: the orchestration file compiles, the external tools
the standard library shells out to are on `PATH`, and — with `--host`, or
auto-discovered from state — that each deploy target is actually reachable
and has a container runtime installed. Most first-deploy failures are
**remote**, not local; this catches them before you run `deploy()` for real.

```
nrg doctor [--file <path>] [--host <host>]...
```

| Flag | Meaning |
| --- | --- |
| `--file <path>` | Path to the `.rhai` file. Defaults to `Energize.rhai` / `energize.rhai`. |
| `--host <host>` | A host to preflight (SSH reachability + container runtime presence). Repeatable. Defaults to every host recorded in `.energize/state.json`, if any have been deployed before — omitted entirely (no host checks run) if there's no state yet and no `--host` given. If `.energize/state.json` *exists* but is corrupt, that's a `doctor` **failure**, not a skip — same as the rest of `nrg` treats a corrupt state file as fatal. |

```bash
nrg doctor                          # after a deploy: hosts auto-discovered from state
nrg doctor --host web1 --host web2  # before the first deploy: name them explicitly
```

```
Energize Doctor

  ✓ Orchestration file found: Energize.rhai
  ✓ Energize.rhai compiles (3 function(s) defined)

  Tools:
  ✓ age found
  ✓ ssh found
  ✓ file transfer: rsync found
  ✓ container runtime: docker found

  Hosts:
  ✓ web1: reachable via SSH
  ✓ web1: container runtime found (/usr/bin/docker)
  ✗ web2: not reachable via SSH

⚠ Some checks failed.
```

What it checks:

- **File compiles.** This is parse-time validation only. Rhai is dynamically
  typed, so this catches **syntax errors**, not runtime or config errors. It
  also does not execute the top level or resolve `import`s.
- **Required tools** must be on `PATH`: `age` and `ssh`.
- **At least one** file-transfer tool: `rsync` or `scp`.
- **At least one** container runtime: `docker` or `podman`.
- **Each host** (from `--host`, or every host recorded in state) is checked
  for SSH reachability first, then — only if reachable — for a container
  runtime binary (`docker`, `podman`, or `nerdctl`) on its `PATH`. Hosts are
  checked in parallel, not one at a time. If neither `--host` nor any deploy
  history exists yet, the host checks are skipped entirely (not a failure).

> **Gotcha:** `nrg doctor` currently treats **`age` as required** and fails the
> whole check if it isn't installed — even if your orchestration uses no
> secrets at all. If you don't use `nrg secrets`, a missing `age` is harmless
> at runtime, but `nrg doctor` will still report `✗ age not found on PATH` and
> exit non-zero.

`nrg doctor` exits `0` only when every check passes; otherwise it prints
`⚠ Some checks failed.` and exits `1`.

---

## `nrg status`

Show what's actually deployed: the version/image recorded in
`.energize/state.json` for a service, plus a live per-host container probe.

```
nrg status [service] [--offline]
```

| Argument / flag | Meaning |
| --- | --- |
| `[service]` | The `service` name passed to `deploy()`. Shows every service found in state if omitted. |
| `--offline` | Skip the live SSH probe; show only what's recorded in state.json. |

```bash
nrg status                # every service found in state
nrg status app            # just "app"
nrg status app --offline  # no network access — state.json only
```

```
app
  version:      v42
  image:        ghcr.io/org/app:v42
  deployed_at:  2026-07-10T08:00:00Z
  previous:     ghcr.io/org/app:v41  (rollback target)
  hosts:
    web1                         target localhost:13000        [running, healthy]
    web2                         target localhost:13010        [unreachable: ssh: connect to host web2 port 22: Connection refused]
```

The live probe runs one `docker inspect` (or the configured runtime's binary
— see `lib/runtime.rhai`) per host over SSH against the canonical container
name `<service>-web`, and reports `running, healthy` / `running, unhealthy` /
`running` (no Docker `HEALTHCHECK` defined) / `stopped` / `not deployed here`
(the host answered SSH but has no container by that name) / `unreachable:
<why>` (SSH itself couldn't connect). A down host is never conflated with a
cleanly stopped or never-deployed container.

`nrg status` never takes the state lock — it only reads `state.json` — so it's
safe to run while a deploy is in progress.

---

## `nrg audit`

Show the history of past `nrg exec`/`nrg run` invocations: who ran what, from
where, and whether it succeeded — recorded automatically in
`.energize/audit.log` by every **live** (non-`--dry-run`) invocation.

```
nrg audit [filter] [--limit N]
```

| Argument / flag | Meaning |
| --- | --- |
| `[filter]` | Only show entries whose target function, args, or file contain this substring. |
| `--limit N` | Show at most N entries, most recent first (default 20; `0` shows all). |

```bash
nrg audit                 # last 20 invocations, most recent first
nrg audit deploy          # only invocations that called/mentioned "deploy"
nrg audit --limit 0       # full history
```

```
2026-07-10T09:00:00Z  maciek@laptop  run deploy v42                                      success
2026-07-09T18:22:04Z  maciek@laptop  run rollback web1 v41                               failed: Pre-deploy release command failed on web1
```

Each entry records a UTC timestamp, `user@host`, the command (`exec`/`run`),
target function and args, and the outcome (`success` or `failed: <reason>`).
Any value the script resolved via `secret()` is redacted from the entry
before it's written — the same boundary the dry-run plan and thrown errors
already go through. `--dry-run` runs write **no** audit entry, matching the
"a dry run touches nothing on disk" contract described in
[Safety Features](safety.md).

---

## `nrg logs`

Tail a service's container logs across its deployed hosts, fanned out over
SSH and prefixed with the host they came from.

```
nrg logs <service> [--host <host>] [--follow] [--lines <n>]
```

| Argument / flag | Meaning |
| --- | --- |
| `<service>` | The `service` name passed to `deploy()`. |
| `--host <host>` | Restrict to one host. Defaults to every host recorded in state for the service. |
| `-f`, `--follow` | Stream new lines as they arrive (like `docker logs -f`). Runs until interrupted. |
| `-n`, `--lines <n>` | Trailing lines to show per host before following. `0` shows the whole log. Default `100`. |

```bash
nrg logs app                    # last 100 lines from every host, then exit
nrg logs app --follow           # keep streaming
nrg logs app --host web1 -n 0   # the whole log, one host only
```

```
web1 | [2026-07-10 09:00:01] Listening on 0.0.0.0:3000
web2 | [2026-07-10 09:00:02] Listening on 0.0.0.0:3000
web1 | [2026-07-10 09:00:15] GET /up 200
```

Runs one `docker logs` (or the configured runtime's binary) per host in
parallel, over a non-interactive SSH connection (matching `RealRunner`'s
`BatchMode`/host-key-checking conventions). Exits non-zero if any host's
connection or log command failed.

---

## `nrg app exec`

Run a command inside a service's **live** container — the running
`<service>-web`, found by looking up the service's hosts in
`.energize/state.json`. This is the console/one-off-command entry point
`nrg ssh` doesn't cover: `nrg ssh` opens a shell on the **host**; `nrg app
exec` runs inside the **container**.

```
nrg app exec <service> [--host <host>] [-i] [cmd...]
```

| Argument / flag | Meaning |
| --- | --- |
| `<service>` | The `service` name passed to `deploy()`. |
| `--host <host>` | Which host to exec into. Required if the service is deployed to more than one host. |
| `-i`, `--interactive` | Allocate a TTY and hand over the terminal — for an interactive shell or console. |
| `[cmd...]` | Command to run inside the container. Defaults to `sh`. A token starting with `-` must follow a literal `--`. |

```bash
nrg app exec app -i                          # drop into a shell
nrg app exec app -i -- bin/rails console     # an interactive Rails console
nrg app exec app -- bin/rails db:migrate:status   # non-interactive; exit code propagates
nrg app exec app --host web2 -i              # pick a host explicitly (required if >1)
```

Without `-i`, the command runs to completion non-interactively (`BatchMode=yes`,
so it can never hang waiting on a password/host-key prompt with nothing
attached to answer it) and its exit code becomes `nrg`'s own exit code — safe
to use in a script or CI. With `-i`, `nrg` replaces itself with `ssh -t ...
docker exec -it ...` (the same process-replacement pattern `nrg ssh` uses),
so the real terminal is handed to the container — and, since it's an
attended session, a host-key or auth prompt is allowed to appear normally.

---

## `nrg remove`

Force-remove a service's own container (`<service>-web`) from each host it's
deployed to, per `.energize/state.json`. The teardown counterpart to
`deploy()` — but scoped to what `deploy()` alone owns per service.

```
nrg remove <service> [--host <host>] [--yes] [--purge-state]
```

| Argument / flag | Meaning |
| --- | --- |
| `<service>` | The `service` name passed to `deploy()`. |
| `--host <host>` | Only remove the container on this host, instead of every host recorded in state. |
| `--yes` | Actually perform the removal. Without it, `nrg remove` only prints what it WOULD remove. |
| `--purge-state` | Also delete this service's per-host state entries (the proxy target) for every host actually removed, once removal succeeds everywhere it was attempted; additionally clears the shared version/image/previous/deployed_at keys, but only if every host the service is recorded as deployed to was covered by this run. |

```bash
nrg remove app                  # preview only — nothing is removed
nrg remove app --yes            # actually remove app-web from every recorded host
nrg remove app --host web2 --yes            # just one host
nrg remove app --yes --purge-state          # remove everywhere, then forget it was ever deployed
```

**Scope.** This deliberately does **not** touch the host's shared proxy
(`kamal-proxy`/`caddy` — one instance serves every service on a host, so
removing it here would take down unrelated services) or accessories (there's
no service-to-accessory mapping recorded anywhere to remove them safely). The
container is force-removed (`docker rm -f`, immediate — no graceful stop
first, the same idiom the stdlib's own `docker_remove` already uses
everywhere), so if the shared proxy is still routing the service's domain to
it, in-flight requests can be dropped; the proxy's route isn't cleaned up
here and keeps pointing at the now-gone container until removed separately.

A container already absent on a host counts as success (idempotent — the
goal state you asked for already holds), whether running Docker or Podman
(`nrg.runtime.cmd`) — both runtimes' "no such container" wording is
recognized. If any host fails, the overall exit code is nonzero and
`--purge-state` is skipped, since state would no longer match reality.
`--host` targeting only some of a multi-host service's hosts never deletes
the shared version/image/previous/deployed_at keys — only the per-host
entries for hosts actually removed — since another, untouched host may still
be running that version.

---

## `nrg ssh`

Open an interactive SSH session to a host, resolving the same aliases your
orchestration scripts use.

```
nrg ssh <host>
```

| Argument | Meaning |
| --- | --- |
| `<host>` | An `~/.ssh/config` alias, or a literal `user@hostname` (required). |

```bash
nrg ssh web1            # resolves the `web1` alias from ~/.ssh/config
nrg ssh deploy@1.2.3.4  # connect literally
```

`nrg ssh` passes `<host>` straight through to the real `ssh` binary — it does
**not** hand-resolve the alias itself, so `ssh`'s own `~/.ssh/config` handling
applies in full (`Port`, `IdentityFile`, `ProxyJump`, `ProxyCommand`, `Host *`
wildcards, `Match` blocks, everything). It then **replaces the current process
with `ssh`** (it `exec`s — it does not return on success).

Before connecting, it also does its own best-effort `HostName`/`User` lookup
purely to print an informational hint — `Connecting to <host>...` if that
lookup didn't change anything, or `Connecting to <host> (resolves to
<hint> per ~/.ssh/config)...` if it did. This hint can be incomplete (it only
understands `HostName`/`User`, not `Port`/`IdentityFile`/`ProxyJump`/etc.) —
it's shown for confirmation only and never affects the actual connection.

---

## `nrg secrets`

Manage encrypted secrets using [`age`](https://github.com/FiloSottile/age).
Secrets are encrypted to a project keypair so ciphertext is safe to commit
while plaintext never lands on disk in the repo.

```
nrg secrets <subcommand>
```

| Subcommand | Purpose |
| --- | --- |
| `nrg secrets init` | Generate a new age keypair (`.nrg-key` + `.nrg-key.pub`) |
| `nrg secrets encrypt [value]` | Encrypt one value → prints an `ENC[...]` token. Omit `value` to read it from stdin instead |
| `nrg secrets decrypt [token]` | Decrypt one `ENC[...]` token → prints plaintext. Omit `token` to read it from stdin instead |
| `nrg secrets seal <file>` | Encrypt an entire file → `<file>.enc` |
| `nrg secrets unseal <file> [--force]` | Decrypt a sealed file → strips the `.enc` suffix. Refuses to overwrite an existing output file unless `--force` is given |

All subcommands shell out to the external `age` / `age-keygen` binaries, so
[`age`](https://github.com/FiloSottile/age) must be installed
(`brew install age` on macOS).

### `nrg secrets init`

```bash
nrg secrets init
```

Generates a keypair **in the current directory**:

- `.nrg-key` — the **private** key. Add it to `.gitignore`; never commit it.
- `.nrg-key.pub` — the **public** key. Safe to commit.

```
  ✓ Generated key pair:
    Private key: /path/to/.nrg-key
    Public key:  /path/to/.nrg-key.pub

  ⚠ Add .nrg-key to your .gitignore!
    The public key (.nrg-key.pub) is safe to commit.
```

Requires `age-keygen` on `PATH`.

### `nrg secrets encrypt`

```bash
nrg secrets encrypt "s3cr3t-db-password"
# or, to keep the value off argv / ps / shell history (recommended):
echo -n "s3cr3t-db-password" | nrg secrets encrypt
```

Encrypts the value to the project public key and prints an armored
`ENC[...]` token on stdout — paste it into config or an env file. Omitting the
positional argument reads the value from stdin instead — a value passed
directly on the command line is visible in `ps` output and shell history.

```
ENC[-----BEGIN AGE ENCRYPTED FILE-----...-----END AGE ENCRYPTED FILE-----]
```

The token is a **single line** (the underlying armor's newlines are joined with
`|` and reversed on decrypt), so it's safe to paste directly into a
`KEY=VALUE` line in `.env` or `.energize/secrets` — `secret("KEY")`
**transparently decrypts** an `ENC[...]` value from either file (or from a
`$NRG_SECRET_KEY` env var) before returning it, using the same key discovery
as `nrg secrets decrypt` below. It throws a clear error if no `.nrg-key` is
found or decryption fails, rather than ever handing back the raw ciphertext.

Looks for the public key by walking up from the current directory for
`.nrg-key.pub`, then falling back to the platform config dir
(`~/.config/nrg/key.pub` on Linux, `~/Library/Application Support/nrg/key.pub` on
macOS). If none is found, it errors and tells you to run `nrg secrets init`.

### `nrg secrets decrypt`

```bash
nrg secrets decrypt 'ENC[...]'
# or via stdin (keeps the ciphertext off argv too):
echo -n 'ENC[...]' | nrg secrets decrypt
```

Strips the `ENC[...]` wrapper, decrypts with the private key, and prints the
plaintext. Looks for the private key by walking up for `.nrg-key`, then the
platform config dir (`~/.config/nrg/key` on Linux, `~/Library/Application
Support/nrg/key` on macOS). A malformed token (not wrapped in `ENC[...]`) is rejected.
Omitting the positional argument reads the token from stdin instead.

### `nrg secrets seal`

```bash
nrg secrets seal .env.production
```

Encrypts a whole file as a single blob to the public key. The output path is
the input path with `.enc` appended (`.env.production` → `.env.production.enc`).
Commit the `.enc`; keep the plaintext out of git.

### `nrg secrets unseal`

```bash
nrg secrets unseal .env.production.enc
```

Decrypts a sealed file with the private key. The output path strips the `.enc`
suffix (`.env.production.enc` → `.env.production`). If the input doesn't end in
`.enc`, the output is written to `<input>.decrypted` instead. Refuses to
overwrite an existing output file (a locally-edited `.env` you haven't re-sealed
yet, say) unless `--force` is passed. The decrypted output is always written
`0600` (owner-only), regardless of the process umask.

---

## Dry-run behavior

`--dry-run` (on `nrg exec` and `nrg run`) records side effects instead of
performing them, then prints a rendered plan. The exact behavior is **per
builtin**:

| Builtin | Dry-run behavior |
| --- | --- |
| `ssh_exec`, `ssh_exec_all`, `ssh_exec_stdin`, `local_exec`, `local_exec_stdin`, `write_remote` | **Recorded, not executed.** Returns a synthetic `ok` result (`exit_code 0`). No SSH/process runs. |
| `ssh_probe` | **Still executes** — it is read-only, so dry-run runs it for real to read live state. |
| `http_get`, `http_post` | **Short-circuited** to a synthetic healthy `200` and recorded. A `wait_healthy`-style loop against a not-yet-started service won't hang or fail the plan. |
| `state_set`, `state_del` | **Recorded.** Applied to an in-memory **overlay** store (never flushed to disk), so subsequent `state_get` stays consistent within the run. |
| `state_get`, `has_state`, `state_all` | Read from the overlay (which started as a copy of real on-disk state). |
| `sleep` | **Skipped** — dry-run does not actually sleep. |

So a dry-run will read real state via `ssh_probe` and the state overlay, assume
HTTP health checks pass, skip sleeps, and log every mutating command it *would*
have run — without taking the lock or writing anything to disk.

> Because dry-run runs `ssh_probe` for real, it still needs SSH connectivity to
> the hosts it probes. It is a *preview of mutations*, not a fully offline
> simulation.

---

## State and locking

A **live** `nrg exec` / `nrg run` (not `--dry-run`):

- Discovers the **project root** by walking up from the current directory for a
  marker: a `.energize/` directory, an `energize.toml` file, or a `.nrg-key`
  file. (`$HOME` is refused as a markerless root, so a stray script can't
  scaffold `$HOME/.energize`.)
- Takes an **advisory file lock** so two live runs can't mutate concurrently.
  If another `nrg` run holds it, you'll see
  `Waiting for the state lock (another nrg run is in progress ...)` and block
  until it's free — indefinitely by default. Pass `--lock-timeout <seconds>`
  to give up after that many seconds instead, surfacing a clear
  `timed out after Ns waiting for the state lock under <root> — another nrg
  run appears to be holding it` error rather than hanging forever (useful for
  CI, where a wedged or crashed prior run should fail the job quickly instead
  of hanging until the runner's own timeout kills it uninformatively). Nested
  `nrg` calls within the same process tree are re-entrant and don't deadlock
  (and aren't subject to `--lock-timeout`, since they never wait on the lock
  at all).
- Loads persistent state from `<root>/.energize/state.json` and flushes
  mutations there atomically.

`--dry-run` skips all of this: no lock, no disk writes, in-memory overlay only.

---

## Exit codes and the failure contract

Every command exits `0` on success, non-zero (usually `1`) on failure. For
`nrg exec` / `nrg run`, "failure" means the script signalled it:

- A Rhai parse error, an uncaught `throw`, or a missing function (for
  `nrg run`) surfaces as an error and exits `1`.
- **A non-zero command does *not* abort the script by itself.** The exec
  builtins fold a failed command into the result's `ok == false` field; they
  return normally. A script signals real failure by checking `.ok` and
  `throw`ing.

The standard library (`lib/*.rhai`) wraps every fallible call with an
`if !r.ok { throw ... }` check, so real deploys exit non-zero on failure. But a
hand-written script that runs `ssh_exec(...)` and **ignores** `r.ok` exits `0`
— by design: it chose not to check. If you care about command failure, either
use the stdlib helpers or check `.ok` yourself:

```rhai
let r = ssh_exec(host, "systemctl restart app");
if !r.ok { throw "restart failed on " + host + ": " + r.stderr; }
```

---

## Environment variables

| Variable | Effect |
| --- | --- |
| `NRG_TRACE` | If set (any value), traces each side-effecting builtin to stderr (with secrets redacted). |
| `NRG_STATE_LOCK` | Set internally to mark a held lock for re-entrancy; you normally don't set this yourself. |

---

## Writing valid Rhai (quick reference)

A few Rhai-specific gotchas that trip people up when authoring orchestration
files:

- **`import` goes at the top level**, never inside a function:
  `import "lib/docker" as docker;`.
- **Config is a map literal** with `#{ ... }`: `#{ image: "nginx", port: 8080 }`.
  There are **no keyword arguments** — pass a map or positional args.
- **Conditions must be `bool`.** `state_get(k)` returns `()` (unit) when the
  key is absent, so test presence with `state_get(k) != ()` or `has_state(k)` —
  **not** `if state_get(k) { ... }`, which raises a runtime type error.
- **Errors are `throw`n** (there is no `fail`): `throw "message";`.
- **`trim()` mutates** the string in place (and returns unit), rather than
  returning a trimmed copy.
- **Secrets can't be string-concatenated.** A resolved secret is a distinct
  type; build commands with `sh_quote(...)` / `reveal(...)` (or deliver it
  off-argv via `ssh_exec_stdin` / `write_remote`) rather than `"... " + secret`.

> **Not in this tool:** there is no Starlark or Bash task runner (both removed —
> orchestration is Rhai only), and no built-in nginx / TLS / provisioning /
> Caddy module. For reverse-proxy needs the supported integration is
> **kamal-proxy**; there is no nginx proxy.
