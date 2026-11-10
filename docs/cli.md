---
title: CLI
parent: Reference
nav_order: 1
---

# CLI

This page lists every `nrg` command, flag, file, environment variable and exit code.

```sh
nrg <command> [options]
nrg --help            # list the commands
nrg <command> --help  # list the flags of one command
```

## All commands

| Command | What it does |
| --- | --- |
| [`nrg run`](#nrg-run) | Call a function defined in your `Energize.rhai`. |
| [`nrg tasks`](#nrg-tasks) | List the functions in your `Energize.rhai`. |
| [`nrg exec`](#nrg-exec) | Run a script from top to bottom. |
| [`nrg init`](#nrg-init) | Create a starter `Energize.rhai`. |
| [`nrg doctor`](#nrg-doctor) | Check that your script compiles and your hosts are ready. |
| [`nrg rehearse`](#nrg-rehearse) | Test a recipe on your own machine in a temporary folder. |
| [`nrg secrets`](#nrg-secrets) | Create a key and store encrypted secrets. |
| [`nrg setup`](#nrg-setup) | Prepare a new host: install Docker, start the proxy. |
| [`nrg status`](#nrg-status) | Show what is deployed and whether it is running. |
| [`nrg audit`](#nrg-audit) | Show the history of past runs. |
| [`nrg logs`](#nrg-logs) | Read a service's container logs from every host. |
| [`nrg app exec`](#nrg-app-exec) | Run a command inside a live container. |
| [`nrg rollback`](#nrg-rollback) | Go back to the previous image. |
| [`nrg remove`](#nrg-remove) | Stop and remove a service's container. |
| [`nrg lock`](#nrg-lock) | Look at, take or release a service's deploy lock. |
| [`nrg ssh`](#nrg-ssh) | Open a shell on a host. |

The examples on this page use one app called `shop`, with the image `ghcr.io/acme/shop` on the hosts `web1` and `web2`.

## Options for every command

### Global option: `--dest <name>`

`--dest` picks a destination, such as `staging` or `production`. Each destination keeps its own state and its own secrets, so a staging deploy never overwrites what production recorded.

```sh
nrg --dest staging status shop
nrg status shop --dest staging     # the same thing
```

You can put `--dest` before or after the command. Give it once. If you give two different values, `nrg` stops with exit code 2:

```text
$ nrg exec --dest a --dest b seed.rhai
Error: conflicting --dest values "a" and "b"; give --dest once
```

A name may contain letters, digits, `-` and `_`. When you use `--dest`, `nrg` prints `[nrg] destination: <name>` on stderr first.

`nrg rehearse` does not accept `--dest`. It always works in a fresh temporary folder. `nrg rollback` must use the same `--dest` as the deploy it undoes. See [Destinations](#destinations).

Every command also accepts `-h`/`--help`. `nrg -V` or `nrg --version` prints the version.

## `nrg run`

Call one function from your script.

```sh
nrg run <fn> [args...] [--file <path>] [--dry-run] [--lock-timeout <seconds>]
```

Save this as `Energize.rhai`:

```rhai
import "std/deploy" as deploy;

const HOSTS = ["web1", "web2"];

fn ship(tag) {
    deploy::deploy(global::HOSTS, "ghcr.io/acme/shop:" + tag, "shop", #{
        container_port: 3000,
        health_path: "/up",
        domain: "shop.example.com",
    });
}
```

Then run the function `ship` with the argument `v42`:

```sh
nrg run ship v42 --dry-run
```

```text
==> Deploying shop v42 to 2 host(s)
  [note] skipping build-arch check under --dry-run (local probe is stubbed)

--- Build ---

--- Push ---

--- Pull ---

--- Proxy ---

--- Deploy (fleet-atomic) ---
...
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `<fn>` | required | The function to call. |
| `[args...]` | none | Arguments for the function. |
| `--file <path>` | `Energize.rhai` | The script to use. |
| `--dry-run` | off | Print the plan and change nothing. No lock, no state writes. See [Dry run](dry-run.md). |
| `--lock-timeout <seconds>` | wait forever | Stop waiting for the state lock after this many seconds. `0` means fail at once if the lock is taken. |

`nrg run` runs the whole top level of the file (imports, constants and any other top-level statements). Then it calls your function.

**Arguments are strings.** `nrg run scale 3` calls `scale("3")`. Convert it inside the function if you need a number.

**An argument that starts with `-` needs `--` before it.** Without the `--`, `nrg` reads it as one of its own flags.

```sh
nrg run note -- -n/a        # calls note("-n/a")
```

**A wrong name stops before anything runs:**

```text
$ nrg run deplooy
Error: no function `deplooy` defined in .../Energize.rhai. `nrg run <fn>` calls a
function; use `nrg exec .../Energize.rhai` to run a top-level script.
```

More examples:

```sh
nrg run ship v42                      # a real deploy
nrg run ship v42 --lock-timeout 60    # give up after 60 s if another run holds the lock
```

There are two different locks. The state lock is a file on your machine (`.energize/state.lock`) that keeps two runs from writing state at once, and `--lock-timeout` is about that one. The deploy lock is a directory on your hosts that keeps two deploys of one service apart; see [`nrg lock`](#nrg-lock) and [Safety](safety.md#locks).

`nrg run` calls only functions defined in your script. To call a function from a module, write a small function that wraps it, like `ship` above.

## `nrg tasks`

List the functions you can call with `nrg run`.

```sh
nrg tasks [--file <path>]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--file <path>` | `Energize.rhai` | The script to read. |

```sh
nrg tasks
```

```text
Functions:
  ship (1 arg)
```

Functions are sorted by name. The argument count shows when it is not zero.

`nrg tasks` only reads the file. It does not run it or follow imports, so it lists only functions written directly in that file. A closure, such as the `transaction(|| { ... })` body, shows up as an `anon$...` entry; ignore those.

## `nrg exec`

Run a script from the first line to the last.

```sh
nrg exec [file] [--dry-run] [--lock-timeout <seconds>]
```

Use `nrg exec` when your script does its work at the top level, with no functions. The framework starters from `nrg init --template rails`, `django`, `nextjs`, `phoenix` and `laravel` work this way. Use [`nrg run`](#nrg-run) when your script has named functions.

| Flag | Default | Meaning |
| --- | --- | --- |
| `[file]` | `Energize.rhai` | The script to run. |
| `--dry-run` | off | Print the plan and change nothing. No lock, no state writes. |
| `--lock-timeout <seconds>` | wait forever | Stop waiting for the state lock after this many seconds. `0` means fail at once. |

```sh
nrg exec                       # run ./Energize.rhai
nrg exec deploy.rhai           # run a different file
nrg exec --dry-run             # preview
nrg --dest staging exec        # use the staging state
```

A real run takes the state lock and writes `.energize/state.json`. A dry run does neither. It still reads from your hosts, so it is not "offline". See [Dry run](dry-run.md).

## `nrg init`

Create a starter `Energize.rhai` in the current folder.

```sh
nrg init [--template <name>]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--template <name>` | the generic starter | One of `release`, `rails`, `django`, `nextjs`, `phoenix`, `laravel`. |

```sh
nrg init
```

```text
✓ Created Energize.rhai
```

The generic starter has two functions, `deploy` and `uptime`. Both talk to `user@example.com`. Change the host before you run it.

| Template | You get |
| --- | --- |
| *(none)* | `deploy()` and `uptime()` over SSH. |
| `release` | A directory release with `deploy(version)` and `rollback(version)`. See [Directory releases](directory-releases.md). |
| `rails`, `django`, `nextjs`, `phoenix`, `laravel` | A full container deploy for that framework. See [Recipes](examples.md). |

```sh
nrg init --template release
nrg tasks
```

```text
Functions:
  config
  deploy (1 arg)
  rollback (1 arg)
```

`nrg init` does not overwrite a file. If `Energize.rhai` exists, it stops with exit code 1 and writes nothing:

```text
Error: Energize.rhai already exists.
```

A template name it does not know is refused by the argument parser (exit code 2) before anything is written.

## `nrg doctor`

Check that your script compiles and that the hosts and tools you name are ready.

```sh
nrg doctor [--file <path>] [--host <host>]... [--checks <file>] [--allow-temporary] [--legacy-tools]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--file <path>` | `Energize.rhai` | The script to compile. |
| `--host <host>` | hosts saved in state | Check this host. Repeat for more. The check is SSH only, unless `--legacy-tools` is set or state records an image for that host. Then it also probes the container runtime and checks registry login (`docker manifest inspect <image>`). |
| `--checks <file>` | none | A JSON file with a list of checks you wrote. See [Testing a deploy](rehearsal.md). |
| `--allow-temporary` | off | Let checks that need a temporary folder run. Without it, those checks fail. |
| `--legacy-tools` | off | Also require `age`, `ssh`, `rsync` or `scp`, and `docker` or `podman` on your `PATH`, and probe each host for a container runtime. |

```sh
nrg doctor --host web1 --legacy-tools
```

```text
Energize Doctor

  ✓ Orchestration file found: /home/you/shop/Energize.rhai
  ✓ [syntax only] /home/you/shop/Energize.rhai compiles (1 function(s) defined)

  Tools:
  ✓ age found
  ✓ ssh found
  ✓ file transfer: rsync, scp found
  ✓ container runtime: docker found
  No deployment-specific checks declared (--checks FILE). Runtime compatibility unverified.

  Hosts:
  ✓ web1: reachable via SSH
  ✓ web1: container runtime found (/usr/bin/docker)

✓ All checks passed!
```

Without `--legacy-tools`, a host with no recorded image gets an SSH check only. It prints `✓ web1: SSH reachable (read-only; runtime not requested)`. A host with a recorded image also gets a runtime probe and a registry-auth check, for example:

```text
  ✓ web1: reachable via SSH
  ✓ web1: container runtime found (/usr/bin/docker)
  ✓ web1: registry auth OK for ghcr.io/acme/shop:v42
```

If a host has no container runtime, the `--legacy-tools` run prints `✗ web1: no docker/podman/nerdctl found on PATH` and exits with code 1.

`nrg doctor` does not run your script and does not follow imports. A pass means only that the checks you asked for passed. It takes no lock and writes no state.

## `nrg rehearse`

Test a recipe on your own machine, in a fresh temporary folder, before you use it on a server.

```sh
nrg rehearse [file] [--dry-run | --execute] [--faults] [--json] [--env <NAME>]...
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `[file]` | `Rehearsal.rhai` | A script that returns a contract. It is not an `Energize.rhai`. |
| `--dry-run` | on | Check the declarations and print the plan. No command runs. |
| `--execute` | off | Run the commands in a disposable local folder. |
| `--faults` | off | Also run the failure scenarios you declared. |
| `--json` | off | Print one JSON report. Command output is not streamed. |
| `--env <NAME>` | none | Pass this environment variable to the commands and hide its value in output. Give the name only. Repeat for more. |

```sh
nrg rehearse                      # check the declarations only
nrg rehearse --execute            # also run the steps
nrg rehearse --execute --faults   # also cause the failures you declared
```

Output for the contract in [Testing a deploy](rehearsal.md#rehearsal-contracts):

```text
$ nrg rehearse --execute --faults
...
bad candidate is rejected / bad candidate is rejected: passed (expected 42, actual 42)
bad candidate is rejected / v1 is still live: passed (expected 0, actual 0)
cleanup / remove release: passed (expected 0, actual 0)
Rehearsal 'shop release': passed
```

`nrg rehearse` does not connect to any host, and it is not a sandbox: your commands keep your normal file and network access. It rejects `--dest` with exit code 2. It creates no lock, no journal and no project state. See [Testing a deploy](rehearsal.md) for how to write a contract.

## `nrg secrets`

Create a key and keep encrypted secrets in your repository.

```sh
nrg secrets init
nrg secrets encrypt <KEY> [--dest <name>] [--print]
nrg secrets decrypt [token]
```

Secrets are encrypted with [age](https://github.com/FiloSottile/age). You need the `age` and `age-keygen` programs on your `PATH` (`brew install age` on macOS). The whole flow, and how scripts read a secret, is in [Secrets](secrets.md).

### `nrg secrets init`

Create the key pair in the current folder.

```sh
nrg secrets init
```

```text
  ✓ Generated key pair:
    Private key: /home/you/shop/.nrg-key
    Public key:  /home/you/shop/.nrg-key.pub

  ⚠ .nrg-key is NOT in .gitignore and you're in a git repo — committing it would leak your private key. Add this line to .gitignore now:
      .nrg-key
    The public key (.nrg-key.pub) is safe to commit.
  Add a secret: printf %s VALUE | nrg secrets encrypt KEY [--dest D]
```

| File | Mode | Commit it? |
| --- | --- | --- |
| `.nrg-key` | `0600` | Never. It is the private key. |
| `.nrg-key.pub` | `0644` | Yes. |

`nrg secrets init` has no flags of its own. It warns you if git does not ignore `.nrg-key`.

### `nrg secrets encrypt`

Read a value from standard input, encrypt it, and save it.

| Flag | Default | Meaning |
| --- | --- | --- |
| `<KEY>` | required | The secret's name. Use letters, digits and `_`, and do not start with a digit. |
| `--dest <name>` | shared file | Save to `.energize/secrets.<name>` instead of `.energize/secrets`. |
| `--print` | off | Print the `ENC[...]` token on stdout. Write no file. |

```sh
printf %s 's3cr3t-db-password' | nrg secrets encrypt DB_PASSWORD
```

```text
  Encrypting to public key: /home/you/shop/.nrg-key.pub
✓ Set DB_PASSWORD in .energize/secrets (ciphertext — safe to commit)
```

The file now holds one line:

```text
DB_PASSWORD=ENC[-----BEGIN AGE ENCRYPTED FILE-----|YWdlLWVuY3J5cHRpb24ub3Jn...]
```

```sh
printf %s 's3cr3t-db-password' | nrg secrets encrypt DB_PASSWORD --dest staging
printf %s 's3cr3t-db-password' | nrg secrets encrypt DB_PASSWORD --print
```

Run it again with the same `KEY` to change the value. The old line is replaced and the other lines stay. On a terminal, `nrg` asks `Value for KEY (end with Ctrl-D):` on stderr.

`nrg secrets encrypt` does not take the value as an argument. A value on the command line would show up in `ps` and in your shell history.

It looks for `.nrg-key.pub` in the current folder and each parent folder, stopping at your home folder. Then it tries `~/.config/nrg/key.pub` (Linux) or `~/Library/Application Support/nrg/key.pub` (macOS). It refuses a key file that another user owns or that anyone can write to.

### `nrg secrets decrypt`

Print one secret as plain text.

| Argument | Default | Meaning |
| --- | --- | --- |
| `[token]` | read from stdin | An `ENC[...]` token. Leave it out to pipe the token in, which keeps it out of `ps` and your shell history. |

```sh
printf %s 's3cr3t-db-password' | nrg secrets encrypt X --print 2>/dev/null | nrg secrets decrypt
```

```text
s3cr3t-db-password
```

It reads the private key with the same search as above (`.nrg-key`, then `~/.config/nrg/key` or `~/Library/Application Support/nrg/key`). A token that is not wrapped in `ENC[...]` is rejected.

## `nrg setup`

Prepare a new host for its first deploy.

```sh
nrg setup --host <host>... [--yes] [--proxy-version <tag>] [--network <name>] [--dry-run] [--file <path>]
```

It checks SSH, installs Docker if it is missing (only with `--yes`), creates a network if you ask, and starts the Caddy proxy that `deploy()` uses.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--host <host>` | required | The host to prepare. Repeat for more. |
| `--yes` | off | Install Docker if the host has no container runtime. It runs the official `https://get.docker.com` script as root over SSH. |
| `--proxy-version <tag>` | `2` | Pin the Caddy image tag, for example `2.8.4`. The default follows Caddy 2. |
| `--network <name>` | none | Create this Docker network on every host. Nothing uses it unless you pass the same name as `cfg.network` to `deploy()`. |
| `--dry-run` | off | Print what would happen. No install, no lock, no state writes. |
| `--file <path>` | `Energize.rhai` | Only marks the project, so state and the audit log land in the right place. `nrg` does not read or run the file. The file must exist; run `nrg init` first. |

```sh
nrg setup --host web1 --dry-run
```

```text
Would install Docker (https://get.docker.com) on: web1

PLAN (dry run — no changes made):
  ssh     web1    docker pull 'caddy:2' [planned; execution-unverified]
  ssh     web1    docker rm -f caddy 2>/dev/null || true [planned; execution-unverified]
  ssh     web1    install -d -m 700 /etc/caddy [planned; execution-unverified]
  write   web1    write 106 bytes -> /etc/caddy/caddy.json [planned; execution-unverified]
  ...
7 action(s), 1 host(s). 0 executed.
```

```sh
nrg setup --host web1                  # start Caddy; stops first if Docker is missing
nrg setup --host web1 --yes            # install Docker if needed, then start Caddy
nrg setup --host web1 --host web2 --network shopnet --yes
```

What happens, in order:

1. Each host is checked for SSH and for a container runtime. An unreachable host stops the command.
2. A host with no runtime and no `--yes` is reported, and nothing else runs (exit code 0).
3. With `--yes`, Docker is installed. If that fails on any host, `nrg` does not touch the network or the proxy on any host.
4. The network is created if you asked, then the proxy starts.

`nrg setup` does not start your accessories (databases and so on). Write a function for that and call it with `nrg run`. It installs Docker only, not Podman. The network and proxy steps run `docker` commands, so a Podman-only host fails there and `nrg setup` warns you first.

## `nrg status`

Show what is deployed and whether it is running.

```sh
nrg status [service] [--offline] [--check] [--json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `[service]` | every service in state | The service name you gave `deploy()`. |
| `--offline` | off | Do not contact the hosts. Show only what `.energize/state.json` recorded. Cannot be combined with `--check`. |
| `--check` | off | Exit with a non-zero code unless every recorded host is running and not unhealthy. |
| `--json` | off | Print a JSON list of services and host probes. |

```sh
nrg status shop
```

```text
shop
  version:      v42
  image:      ghcr.io/acme/shop:v42
  deployed_at:      2026-11-10 17:33:51 UTC
  previous:      ghcr.io/acme/shop:v41
  hosts:
    web1                         target localhost:13000        [running, healthy]
    web2                         target localhost:13000        [running, healthy]
```

Each host shows one of these:

| Shown | Meaning |
| --- | --- |
| `running, healthy` | The container runs and its health check passes. |
| `running, unhealthy` | The container runs but its health check fails. |
| `running` | The container runs and has no health check, or the check is still starting. |
| `stopped` | The container exists but is not running. |
| `not deployed here` | The host answered, but has no container with that name. |
| `unreachable: <why>` | SSH could not connect. |
| `probe failed (exit N): <why>` | The runtime is missing or gave output `nrg` does not understand. |
| `offline` | You used `--offline`, so nothing was probed. |

```sh
nrg status                  # every service
nrg status shop --offline   # no network, state only
nrg status shop --check     # exit code 1 if anything is down
nrg status shop --json      # for scripts
```

`nrg status` reads container deployments. It does not know about directory releases. It never takes the state lock, so it is safe to run during a deploy. Without `--check`, the exit code stays 0 even when a host is down.

## `nrg audit`

Show the history of past `nrg exec`, `nrg run` and `nrg rollback` calls: who ran what, when, and how it ended.

```sh
nrg audit [filter] [--limit <n>] [--verbose] [--json]
nrg audit [--run <id> | --incomplete] [--limit <n>] [--json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `[filter]` | none | Show entries that contain this text in the function, arguments, file, run ID, step, host or error. |
| `--limit <n>` | `20` | Show at most `n` entries, newest first. `0` shows all. |
| `--verbose` | off | Also show successful steps. Failures always show. |
| `--json` | off | Print JSON. An empty result is `[]`. |
| `--run <id>` | none | Show the step-by-step events of one run. Cannot be combined with `--incomplete`. |
| `--incomplete` | off | Show runs that have no finish event. They may still be running, or were interrupted. |

```sh
nrg audit              # the last 20 runs
nrg audit ship         # runs that mention "ship"
nrg audit --limit 0    # everything
nrg audit --incomplete
nrg audit --run RUN_ID --json
```

```text
2026-11-10T17:33:51Z  root@laptop  run ship v42      success
  run: 18e6bf79c568688d-1526968
2026-11-10T17:33:51Z  root@laptop  run ship v41      success
  run: 18e6bf79a0f79e4d-1526748
```

The `run:` line is the ID you give to `nrg audit --run`.

Each entry has a UTC time, `user@host`, the command, the function and arguments, and the result. Values read with `secret()` are replaced before anything is written.

`nrg audit` does not record dry runs. A dry run writes nothing to disk. Text that came back from a host is printed with control characters escaped, so a host cannot repaint your terminal.

`nrg remove`, `nrg lock`, `nrg app` and `nrg secrets` also add `begin` and `success`/`failed (exit N)` lines to the audit log, and so does `nrg setup` unless it is a dry run.

## `nrg logs`

Read a service's container logs from every host, each line starting with the host name.

```sh
nrg logs <service> [--host <host>] [--follow] [--lines <n>]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `<service>` | required | The service name you gave `deploy()`. |
| `--host <host>` | every recorded host | Read from one host only. |
| `-f`, `--follow` | off | Keep streaming new lines until you press Ctrl-C. |
| `-n`, `--lines <n>` | `100` | Lines to show from each host first. `0` shows the whole log. |

```sh
nrg logs shop -n 5
```

```text
web2 | [2026-11-10 09:00:01] Listening on 0.0.0.0:3000
web1 | [2026-11-10 09:00:01] Listening on 0.0.0.0:3000
```

```sh
nrg logs shop --follow            # stream from every host
nrg logs shop --host web1 -n 0    # the whole log of one host
```

The hosts are read in parallel, so lines from different hosts can come in any order. The exit code is non-zero if any host fails.

## `nrg app exec`

Run a command inside a service's live container. Use `nrg ssh` for a shell on the host instead.

```sh
nrg app exec <service> [--host <host>] [-i] [cmd...]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `<service>` | required | The service name you gave `deploy()`. |
| `--host <host>` | the only host | The host to use. Required when the service runs on more than one host. |
| `-i`, `--interactive` | off | Give the command a terminal, for a shell or a console. |
| `[cmd...]` | `sh` | The command to run. Put `--` before any part that starts with `-`. |

```sh
nrg app exec shop --host web1 -i                       # a shell in the container
nrg app exec shop --host web1 -i -- bin/rails console  # an interactive console
nrg app exec shop --host web1 -- ls /app               # run once and return
```

Without `-i`, the command runs to the end without a terminal, and its exit code becomes the exit code of `nrg`. That makes it safe in scripts and CI. With `-i`, `nrg` hands your terminal to the container.

## `nrg rollback`

Put a service back on its previous image.

```sh
nrg rollback <service> [--host <host>]... [--image <image>] [--dry-run] [--lock-timeout <seconds>] [--file <path>]
```

You do not need a `rollback` function in your script. `nrg` calls the standard library's rollback for you.

| Flag | Default | Meaning |
| --- | --- | --- |
| `<service>` | required | The service name you gave `deploy()`. |
| `--host <host>` | every recorded host | Roll back this host only. Repeat for more. |
| `--image <image>` | the saved previous image | Roll back to this exact image. An empty value is refused, so an unset shell variable cannot fall back silently. |
| `--dry-run` | off | Print the plan and change nothing. No lock, no state writes. |
| `--lock-timeout <seconds>` | wait forever | Stop waiting for the lock after this many seconds. |
| `--file <path>` | `Energize.rhai` | Only marks the project. `nrg` does not read or run the file. |

```sh
nrg rollback shop --dry-run
```

```text
==> Rolling back shop to ghcr.io/acme/shop:v41
==> Deploying shop v41 to 2 host(s)

--- Pull ---

--- Proxy ---

--- Deploy (fleet-atomic) ---

  [web1]
    starting shop-web-v41-<auto> on port <auto>
    waiting for health...
  health check passed on web1 (1/30, 1/1 consecutive)
    switching traffic -> localhost:<auto>
...
==> Deploy complete: shop v41
```

(A dry run still reads from your hosts, but only simple reads such as container probes really run. Health checks and other waits are assumed to pass. Nothing is changed.)

```sh
nrg rollback shop                                  # to the saved previous image
nrg rollback shop --host web2                      # one host
nrg rollback shop --image ghcr.io/acme/shop:v41    # a specific image
nrg --dest staging rollback shop                   # use the same --dest as the deploy
```

If no hosts are recorded and you gave no `--host`, `nrg` stops with an error. It will not roll back to a mutable `:latest` tag that it saved on its own. An image you pass with `--image` is used as given.

A rollback is a real deploy, so it takes the state lock and the deploy lock, handles Ctrl-C, and appears in the audit log.

For directory releases, call your own `rollback` function with `nrg run rollback <version>`. See [Directory releases](directory-releases.md).

## `nrg remove`

Stop and remove a service's container from its hosts.

```sh
nrg remove <service> [--host <host>] [--yes] [--purge-state]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `<service>` | required | The service name you gave `deploy()`. |
| `--host <host>` | every recorded host | Remove from this host only. |
| `--yes` | off | Do it. Without `--yes`, `nrg` only prints what it would remove. |
| `--purge-state` | off | Also delete the service's entries in `.energize/state.json` once removal worked everywhere. |

```sh
nrg remove shop
```

```text
Would remove "shop-web" from:
  web1
  web2
Re-run with --yes to actually remove it. This does not touch the shared proxy or any accessories on these hosts.
```

```sh
nrg remove shop --yes                  # remove from every host
nrg remove shop --host web2 --yes      # one host
nrg remove shop --yes --purge-state    # remove, then forget the deploy
```

`nrg remove` does not touch the shared Caddy proxy or any accessories. The container is force-removed at once, so requests in flight can be dropped, and the proxy keeps its route to the old container until you change it.

A container that is already gone counts as removed. If any host fails, the exit code is non-zero and `--purge-state` is skipped. With `--host`, the shared version and image keys stay, because other hosts may still run that version.

## `nrg lock`

Look at, take or release the lock that `deploy()`, `rollback()` and `nrg remove --yes` hold while they work. The lock stops two deploys of the same service from running at once.

```sh
nrg lock status  <service> [--host <host>]
nrg lock acquire <service> [--host <host>]
nrg lock release <service> [--host <host>] [--yes]
```

### `nrg lock status|acquire|release`

| Command | What it does |
| --- | --- |
| `nrg lock status <service>` | Say whether the lock is held and by whom. Reads only. |
| `nrg lock acquire <service>` | Take the lock yourself, for example during maintenance, so no deploy can start. It does not deploy anything. |
| `nrg lock release <service>` | Force the lock open. Without `--yes` it only prints what it would release. |

| Flag | Default | Meaning |
| --- | --- | --- |
| `<service>` | required | The service name you gave `deploy()`. |
| `--host <host>` | the host recorded in state | The one host to look at. Required when the service has zero or several recorded hosts. |
| `--yes` | off | Only for `release`. Actually release the lock. |

```sh
nrg lock status shop --host web1
nrg lock acquire shop --host web1      # maintenance window starts
nrg lock release shop --host web1      # preview: prints what would be released
nrg lock release shop --host web1 --yes
```

The lock is the directory `/tmp/nrg-deploy-lock-<service>`. `deploy()`, `rollback()` and `nrg remove --yes` create it on every host of the service. `nrg lock` works on one host at a time, so pass `--host` to say which one.

`nrg lock release` can break a live deploy. Release a lock only when you are sure the run that took it has crashed.

## `nrg ssh`

Open a shell on a host.

```sh
nrg ssh <host>
```

| Argument | Meaning |
| --- | --- |
| `<host>` | A name from `~/.ssh/config`, or `user@hostname`. |

```sh
nrg ssh web1
nrg ssh deploy@203.0.113.7
```

`nrg ssh` hands the host name straight to the system `ssh`, so everything in your `~/.ssh/config` applies: `Port`, `IdentityFile`, `ProxyJump` and the rest. It replaces the `nrg` process with `ssh`. It prints one line first (`Connecting to web1...`) so you can see where it goes.

It does not open a shell inside a container. Use [`nrg app exec`](#nrg-app-exec) for that.

## Files nrg creates

`nrg` finds your project root by looking in the current folder and then in each parent folder for `.energize`, `energize.toml` or `.nrg-key`. Inside your home folder, it stops at your home folder. If it finds none, the current folder is the root.

| File | Made by | What it holds | Commit it? |
| --- | --- | --- | --- |
| `Energize.rhai` | `nrg init` | Your script. | Yes |
| `.nrg-key` | `nrg secrets init` | The private key. | Never |
| `.nrg-key.pub` | `nrg secrets init` | The public key. | Yes |
| `.energize/secrets` | `nrg secrets encrypt` | Encrypted secrets shared by all destinations. | Yes |
| `.energize/secrets.<dest>` | `nrg secrets encrypt --dest` | Encrypted secrets for one destination. | Yes |
| `.energize/state.json` | `deploy()` and `state_set` | Version, image, previous image and port of each service. Plus `state.json.bak`. | No |
| `.energize/audit.log` | `nrg run`, `nrg exec`, `nrg rollback`, `nrg secrets`, `nrg remove`, `nrg lock`, `nrg app`, `nrg setup` (not a dry run) | Who ran what and the result. | No |
| `.energize/runs.jsonl` | `nrg run`, `nrg exec`, `nrg rollback` | One event per step, for `nrg audit --run`. | No |
| `.energize/state.lock`, `.energize/state.write.lock` | any live run | Keep two runs from writing state at once. | No |

To commit the encrypted secrets and ignore the rest, use the `.gitignore` block in [Secrets](secrets.md#what-to-commit).

On your hosts, `nrg` creates `/tmp/nrg-deploy-lock-<service>` (the deploy lock) and, with `nrg setup` or `deploy()`, `/etc/caddy/caddy.json` (the proxy config).

## Destinations

A destination is a name such as `staging` that you give with `--dest`. It keeps the state and the secrets of one environment apart from the others. State stays in the same `.energize/state.json`, under keys that start with the destination name (for example `staging/shop.version`). There is no separate state file per destination. Secrets do get their own file: `.energize/secrets.staging`.

Give each destination its own hosts. The container is always named `<service>-web`, so two destinations on one host would replace each other's container. How to write a script that changes behavior per destination is in [Writing scripts](authoring.md#staging-and-production---dest).

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | It worked. |
| `1` | It failed: a script error, a failed check, a host that failed, `nrg status --check` found a problem, or `nrg init` found a file already there. |
| `2` | `nrg` could not use your arguments: an unknown flag or value, two different `--dest` values, or a rehearsal contract that is not valid. |
| `130` | You pressed Ctrl-C (SIGINT). Also used by `nrg rehearse` when interrupted. |
| `143` | The process was stopped with SIGTERM. |
| *other* | `nrg app exec` without `-i` returns the exit code of your command. |

A script that calls `ssh_exec` and ignores `.ok` exits 0 even when the remote command failed. Check `.ok` yourself, or use `ssh_step`, which throws on failure:

```rhai
let r = ssh_exec("web1", "systemctl restart shop");
if !r.ok { throw "restart failed: " + r.stderr; }
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `NRG_SECRET_<NAME>` | The plain value of a secret, for CI. `NRG_SECRET_DB_PASSWORD` is what `secret("DB_PASSWORD")` reads first. |
| `NRG_SSH_HOST_KEY_CHECKING` | The `StrictHostKeyChecking` mode for every SSH connection. Default `yes`: a host missing from `known_hosts` is refused. Also accepts `accept-new`, `no`, `off` and `ask`. Any other value means `yes`. |
| `NRG_SSH_CONTROL_PERSIST` | How long a shared SSH connection stays open. Default `60s`. `no`, `0` or `off` turns sharing off. |
| `NRG_SSH_CONTROL_PATH` | Where the shared SSH sockets live. It must contain `%C` or `%h`, otherwise it is ignored. Default `~/.ssh/nrg-cm/%C`. |
| `NRG_COMMAND_TIMEOUT_SECS` | How long one command may run, in seconds. Default `600`. A step's `timeout_secs` overrides it. |
| `NRG_MAX_OUTPUT_BYTES` | The most output `nrg` keeps from one command, per stream. Default 16 MiB. |
| `NRG_TRACE` | Set it to anything to print a trace of the commands `nrg` runs. Secrets are hidden. |
| `NRG_STATE_LOCK` | `nrg` sets this itself so a nested `nrg` can reuse the lock. Do not set it. |
| `USER` | Your name in the audit log and in lock holder files. |
| `HOME` | Where `nrg` finds `~/.ssh` and where it stops looking for a project root. |

Scripts can read any variable with `env_or("NAME", "default")` or `nrg_env("NAME")`. See [Builtins](builtins.md). Inside `nrg rehearse --execute`, commands also see `NRG_WORKSPACE`, `NRG_SOURCE` and `NRG_BIN`.

## Next

- Write your own deploy: [Writing scripts](authoring.md)
- See what each function does: [Builtins](builtins.md) and [Standard library](stdlib.md)
- Learn what is safe to run: [Safety and limits](safety.md)
