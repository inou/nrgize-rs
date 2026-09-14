---
title: Getting Started
nav_order: 2
---

# Getting Started with Energize (`nrg`)

Energize is a deployment toolkit written in Rust with a [Rhai](https://rhai.rs)
orchestration engine. You write your deployment as a `.rhai` script; `nrg` evaluates it
top-to-bottom, and the built-in functions (`ssh_exec`, `http_get`, `state_set`, …) have
**real side effects** as evaluation reaches them. The core works with any toolchain.
Optional recipes provide directory releases, container rollouts and framework build defaults.

There is one engine, two ways to drive it:

- **`nrg exec [file]`** evaluates a module top-to-bottom (defaults to `Energize.rhai`).
- **`nrg run <fn> [args...]`** loads the same file, then **calls a function** defined in it.

This guide takes you from installation to a customizable deployment workflow. For the full
reference, see the linked pages at the [bottom](#where-to-go-next).

---

## Install

Prebuilt binaries (roadmap 3.1) for macOS (arm64/x86_64) and Linux (x86_64/arm64) are built by
`.github/workflows/release.yml` on every tagged release and published to [GitHub
Releases](https://github.com/inou/nrgize-rs/releases):

```bash
curl -fsSL https://raw.githubusercontent.com/inou/nrgize-rs/main/scripts/install.sh | sh
```

This detects your OS/arch, downloads the matching release asset, verifies its sha256
checksum, and installs to `~/.local/bin` (`--bin-dir DIR` / `$NRG_INSTALL_DIR` to change
that, `--version vX.Y.Z` / `$NRG_VERSION` to pin a version instead of the latest release).

Or install with Homebrew using this repository as the tap:

```bash
# Trust only this formula on Homebrew versions that require tap trust.
if brew help trust >/dev/null 2>&1; then brew trust --formula inou/nrg/nrg; fi
brew tap inou/nrg https://github.com/inou/nrgize-rs.git
brew install inou/nrg/nrg
```

To upgrade a Homebrew installation, run `brew update && brew upgrade inou/nrg/nrg`.

Alternatively, build from source with a **recent stable Rust** toolchain (install via
[rustup](https://rustup.rs)):

```bash
git clone https://github.com/inou/nrgize-rs.git
cd nrgize-rs
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

Confirm it runs:

```bash
nrg --help
```

These docs track `main`. A tagged release or installed binary can lag behind the
latest recipes and flags; check `nrg --help` and build from `main` when evaluating
new features that are not yet in a release.

### Homebrew troubleshooting

- **Repository not found:** `brew tap inou/nrg` assumes a GitHub repository named
  `inou/homebrew-nrg`. This project hosts its formula in `inou/nrgize-rs`, so the
  explicit URL above is required when first adding the tap.
- **Untrusted formula / invalid syntax in tap:** recent Homebrew versions check
  trust during tap validation. Run `brew trust --formula inou/nrg/nrg` before
  retrying the explicit tap command. This trusts the named formula without
  enabling all formulas or commands in the tap. Older Homebrew versions have no
  `trust` command; the guarded installation command handles both.
- **Upgrade does nothing:** `brew update` refreshes Homebrew and tap definitions;
  `brew upgrade inou/nrg/nrg` upgrades the installed package. It installs the
  release version pinned in `Formula/nrg.rb`, which can lag behind `main`.
- **The reported version is still old:** run `type -a nrg`, then compare
  `nrg --version` with `"$(brew --prefix)/bin/nrg" --version`. A binary installed
  earlier by the shell installer or a source build may appear first on `PATH`.

To switch a previous `~/.local/bin/nrg` installation to Homebrew, preserve the old
binary before replacing it with a symlink (only do this if that is the path
reported above):

```bash
mv -i "$HOME/.local/bin/nrg" "$HOME/.local/bin/nrg.before-homebrew"
# Continue only after the backup succeeds and the old path is vacant.
ln -s "$(brew --prefix)/bin/nrg" "$HOME/.local/bin/nrg"
hash -r
nrg --version
```

Future Homebrew upgrades follow that symlink automatically. No shell startup
file changes are needed. See Homebrew's [tap documentation](https://docs.brew.sh/Taps)
and [formula trust documentation](https://docs.brew.sh/Tap-Trust).

### Optional external tools

`nrg` shells out to a few standard CLIs. Install only the ones your deploy actually uses.

| Tool      | Needed for                                   | Install                                       |
|-----------|----------------------------------------------|-----------------------------------------------|
| `ssh`     | Remote execution (`ssh_exec`, `nrg ssh`)     | Part of OpenSSH (usually pre-installed)        |
| `age`     | Encrypted secret management (`nrg secrets`)  | `brew install age` / `apt install age`         |
| `rsync`   | Scripts that explicitly invoke rsync                    | Usually pre-installed                          |
| `scp`     | Scripts that explicitly invoke scp                     | Part of OpenSSH                                |
| `docker`  | Container deployments                        | <https://docs.docker.com/get-docker>           |
| `podman`  | Container deployments (alternative)          | <https://podman.io/getting-started>            |

On macOS, [OrbStack](https://orbstack.dev) works as a Docker variant and is auto-detected.

### `nrg doctor`

`nrg doctor` compiles the orchestration file without evaluating it. With no
capability declarations, it reports runtime compatibility as unverified. It does
not require age or a container runtime for a fresh non-container deployment.
Hosts named with `--host` receive reachability probes; recorded container state
can add runtime/image checks. Use `--legacy-tools` only for the old blanket checks.

```bash
nrg doctor
nrg doctor --checks checks.json
nrg doctor --checks checks.json --allow-temporary
```

A `checks.json` can declare exactly what this deployment needs:

```json
[
  {"name":"SSH client", "kind":"read-only", "command":"ssh -V"},
  {"name":"Destination file permissions", "kind":"file-permissions", "host":"deploy@web1", "directory":"/srv/myapp"}
]
```

Read-only probes execute commands. The permission check requires
`--allow-temporary`, creates an isolated directory on the named filesystem, and
cleans up afterward. Plain syntax validation does not test runtime behavior.
See [preflight declarations](builtins.md#preflightchecks-allow_temporary).

`upload_file` and `download_file` use SSH and remote POSIX tools with explicit
permissions; they do not require rsync or scp.

---

## Scaffold a file with `nrg init`

`nrg init` writes a starter `Energize.rhai` in the current directory. It refuses to
overwrite an existing one.

```bash
nrg init
```

The scaffold is a minimal, dependency-free script — no stdlib import, just two functions
over the SSH builtins:

```rhai
// Energize.rhai — Rhai orchestration module.
//
//   nrg run <fn> [args]   call a function defined here
//   nrg exec              run this file top-to-bottom
//   nrg exec --dry-run    show the plan without executing

const HOSTS = ["user@example.com"];

// `nrg run deploy`
fn deploy() {
    for host in global::HOSTS {
        let r = ssh_exec(host, "cd /var/www/app && git pull origin main");
        if !r.ok { throw "deploy failed on " + host + ": " + r.stderr; }
    }
    print("Deployed to all hosts.");
}

// `nrg run uptime`
fn uptime() {
    ssh_exec_all(global::HOSTS, "uptime");
}
```

For directory releases use `nrg init --template release`. For a container starter,
choose `rails`, `django`, `nextjs`, `phoenix` or `laravel`. All use embedded modules
and work without vendoring. The [workflow guide](workflows.md) explains the choice.

List the entry points it defines (each `fn` is a `nrg run` target):

```bash
nrg tasks
```

```
Functions:
  deploy
  uptime
```

`nrg` discovers the file as `Energize.rhai` (or `energize.rhai`) in the current directory.
Pass an explicit path to `nrg exec <file>`, or `--file <path>` to
`nrg run` / `nrg tasks` / `nrg doctor`.

---

## `nrg exec` vs `nrg run`

Both commands build the same engine and run the **top level** of the file first (so
`import`s and top-level `let`/config statements execute, with their side effects). They
differ in what happens after that.

### `nrg exec [file]` — run a module top-to-bottom

`nrg exec` evaluates the whole file in order and stops. This is the right command when your
script **is** the deploy: top-level statements call into the stdlib and the deploy happens
as evaluation reaches them. The framework examples in `lib/examples/` are written this way.

```bash
nrg exec                 # runs ./Energize.rhai top-to-bottom
nrg exec deploy.rhai     # runs a specific file
```

### `nrg run <fn> [args...]` — call a function

`nrg run` runs the top level (imports, config), then **calls the named function**. Use it
when your file defines several entry points (like the `deploy` / `uptime` scaffold) and you
want to pick one.

```bash
nrg run deploy
nrg run uptime
```

Two things to know about arguments:

- **Every trailing CLI argument is passed as a Rhai _string_.** If your function needs a
  number, coerce it inside the function (e.g. `parse_int(n)`). There are no typed args.
- A function argument that itself starts with `-` must come after a `--` separator, so it
  isn't mistaken for a flag:

  ```bash
  nrg run scale web -- --replicas=3
  ```

`nrg run` refuses up front if no function by that name is defined — so it will never
accidentally run a top-level deploy while looking for a function that doesn't exist.

> **Gotcha — top level runs either way.** Because `nrg run` evaluates the top level first,
> any *side-effecting* statement you put at the top level (not inside a `fn`) runs even when
> you only meant to call one function. Keep top-level code to imports and configuration;
> put effects inside functions if you use `nrg run`.

---

## A first end-to-end example

This directory-release workflow accepts any staged application artifact. Replace
the host, paths, supervisor commands and health endpoint for your app before a
live run. Prepare runs inside a new release directory; activation and health run
from the application root.

```rhai
import "std/release" as release;

fn config() {
    #{
        prepare: "cp -R /srv/staged/api/. .",
        activate: "sudo -n systemctl restart api",
        health: "curl --fail --silent --show-error http://127.0.0.1:8080/health",
        deactivate: "sudo -n systemctl stop api",
        timeout_secs: 120
    }
}
fn deploy(version) {
    release::deploy("deploy@web1", "/srv/api", version, config());
}
fn rollback(version) {
    release::rollback("deploy@web1", "/srv/api", version, config());
}
```

```bash
nrg run deploy v42 --dry-run
# After inspecting the plan and probing the required host capabilities:
nrg run deploy v42
nrg audit --verbose
# To activate a chosen retained release:
nrg run rollback v41
```

The lifecycle switches `/srv/api/current` and attempts to restore the previous
release if activation or health fails. It retains release directories, rejects
rebuilding an existing version, and does not undo database migrations. No runtime
or supervisor is installed by this helper. Add a framework build recipe or custom
build command as needed; see [workflows](workflows.md). For container deployment,
start with [framework examples](examples.md) or the [container guide](deploy.md).

### Using the stdlib

The stdlib is embedded in the `nrg` binary — `import "std/docker" as docker;` etc. works with
**zero setup**, no vendoring required, version-locked to the binary. Prefer this for a script you
write yourself.

The container framework examples use the on-disk
`import "lib/…"` convention, so copying one by hand needs the stdlib vendored as a
**sibling** directory (import paths resolve relative to the script's own directory):

```bash
nrg vendor
# Copy the desired example from the nrg source tree into this project as Energize.rhai.
```

`nrg init --template rails|django|nextjs|phoenix|laravel` (roadmap 3.4) does this in one
step, with the `recipe` import already switched to `import "std/…"` — no vendoring
required. See [`nrg init`](cli.md#nrg-init) and [Framework examples](examples.md).

`nrg vendor [--force]` does the same as `cp -r lib ./lib` — materializing the embedded stdlib
onto disk — and is only needed if you want to customize a module's behavior; edit the vendored
copy and switch that one import from `"std/X"` to `"lib/X"` (`std/X` always selects the embedded copy; `lib/X` selects your file).

The library includes general `release` and `release_recipes` modules, app-scoped
`mise` provisioning, container/runtime/proxy modules, notifications, and Bunny
Magic Containers support. Container deployment supports kamal-proxy or Caddy;
`nrg setup` handles the documented container-host setup flow. These are optional
choices, not prerequisites for a directory release. See [stdlib.md](stdlib.md).

---

## Preview with `--dry-run`

Before any real run, plan it. `--dry-run` (available on both `nrg exec` and `nrg run`)
intercepts effects instead of performing them and prints the plan at the end.

```bash
nrg exec --dry-run
nrg run deploy v42 --dry-run
```

```
PLAN (dry run — no changes made):
  ssh     deploy@web1            mkdir ... [planned; execution-unverified]
  ssh     deploy@web1            Prepare release ... [planned; execution-unverified]
  ...
N action(s), M host(s). 0 executed.
```

What dry-run actually does, by effect type — worth knowing so the plan reads correctly:

- **Mutating builtins** (`ssh_exec`, `local_exec`, `write_remote`, container/proxy
  `sim_*` mutations, `state_set`/`state_del`) **record** a planned action and return a
  synthetic success (`ok`). They update an in-memory **simulation overlay**, so a stubbed
  `sim_docker_run` makes a later `sim_container_running` read return `true` — reads-after-writes
  stay consistent.
- **Reads** route through that overlay (seeded lazily from one real probe per container).
- **`http_get` / `http_post` short-circuit** to a healthy synthetic `200` — health checks
  pass in a plan.
- **`sleep` is skipped** (no real delay).
- A dry run takes **no deployment locks** and writes **no state or run journal**.

`ssh_probe` is read-only but still *runs* under `--dry-run` (it is not a mutation). Keep
that in mind if a probe touches something slow or sensitive.

> Dry runs check orchestration shape and ordering. Shell operations are
> **planned; execution-unverified**. Use explicit preflight probes to test actual
> capabilities; a synthetic success does not establish command compatibility.

A live (non-dry) run does the opposite: it resolves the project root, takes an exclusive
advisory lock on `<root>/.energize/state.lock` for the duration, and persists state
atomically. Concurrent mutating runs serialize. See [safety.md](safety.md).

---

## Where to go next

- **[workflows.md](workflows.md)** — generic releases, optional framework defaults,
  execution options, cancellation and run history.

- **[cli.md](cli.md)** — every command and flag (`exec`, `run`, `tasks`, `ssh`, `init`,
  `doctor`, `secrets`).
- **[builtins.md](builtins.md)** — the runtime builtins (`ssh_exec*`, `http_*`, `state_*`,
  `secret`/`reveal`/`sh_quote`, `transaction`/`on_rollback`, the `sim_*` family) with exact
  signatures.
- **[stdlib.md](stdlib.md)** — the `lib/` modules: runtime, docker, proxy, healthcheck,
  registry, deploy.
- **[deploy.md](deploy.md)** — how the fleet-atomic `deploy()` / `rollback()` /
  `accessory_run()` workflow behaves end-to-end.
- **[safety.md](safety.md)** — dry-run, state locking, atomic state writes, secret handling,
  and transactional rollback.
- **[authoring.md](authoring.md)** — writing valid Rhai for `nrg`: config maps, the `Secret`
  type, `state_get` returning `()` for absent keys, per-file imports, and other gotchas.
