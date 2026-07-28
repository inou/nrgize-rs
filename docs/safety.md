---
title: Safety Features
nav_order: 7
---

# Production safety

Energize (`nrg`) drives real infrastructure: it runs commands over SSH, writes
files to remote hosts, swaps proxy targets, and records deploy history to disk.
Four mechanisms make that safe to do from a laptop against production:

1. **Dry-run** — see the exact plan a run would execute, with reads kept
   self-consistent against simulated writes, and zero side effects.
2. **State locking** — a single advisory lock per project root plus atomic,
   crash-safe writes, so concurrent runs can't corrupt or lose deploy history.
3. **Secrets** — a tagged `Secret` type that can't be printed, concatenated, or
   stored, with redaction as a backstop on every output boundary.
4. **Transactions** — a compensation stack that unwinds completed steps in
   reverse order when a deploy throws partway through — including a Ctrl-C
   (SIGINT/SIGTERM) mid-run, which triggers the same unwind rather than
   killing the process with zero cleanup.

Each section below describes what the feature guarantees *and* where the
guarantee stops. None of these is magic; the limits are as important as the
guarantees.

---

## 1. Dry-run

Run any orchestration file with `--dry-run` to see what it *would* do without
touching anything:

```sh
nrg exec deploy.rhai --dry-run
```

The flag is wired in `src/cli/exec.rs`:

```rust
/// Show the plan of side effects without executing (no lock, no state writes).
#[arg(long)]
pub dry_run: bool,
```

### Effect interception is per-builtin, not per-command-string

`nrg` does not parse your shell commands to guess whether they mutate. Instead,
each side-effecting *builtin* declares its own behavior. In dry-run mode
(`EffectMode::DryRun`), a **mutating** builtin records a `PlannedAction` and
returns a synthetic-success result instead of running anything; a **read-only**
builtin still runs (or seeds from a single real probe — see below).

The classification, straight from the code:

| Builtin | Class | Dry-run behavior |
|---|---|---|
| `ssh_exec(host, cmd)` | mutating | records `ssh`, returns synthetic ok |
| `ssh_exec_all(hosts, cmd)` | mutating | records one `ssh-all` per host |
| `ssh_exec_stdin(host, cmd, stdin)` | mutating | records `ssh-stdin` (payload never recorded) |
| `local_exec(cmd)` | mutating | records `local` |
| `local_exec_stdin(cmd, stdin)` | mutating | records `local-stdin` |
| `write_remote(host, content, path)` | mutating | records `write` as `write N bytes -> path` |
| `state_set` / `state_del` | mutating | records `state`, applies to in-memory overlay |
| `sim_docker_run` / `sim_docker_stop` / `sim_*` mutations | mutating | records + updates the sim model |
| `ssh_probe(host, cmd)` | read-only | **still runs the real command** |
| `sim_container_running` / `sim_container_healthy` / `sim_image_id` | read-only | seed once from a real probe, then read the sim |
| `http_get` / `http_post` | read-only | short-circuit to synthetic `200`, record a `check` |
| `sleep(seconds)` | — | skipped entirely in dry-run |

Two consequences worth internalizing:

- **`ssh_probe` runs for real, even in dry-run.** It is the read-only escape
  hatch, so a dry-run can read live state to plan against it. If you put a
  *mutation* behind `ssh_probe`, dry-run will execute it. Mutations belong in
  `ssh_exec` / the `sim_*` mutators.
- A hand-written script that wraps a write in something other than these
  builtins (e.g. you shell out to a tool that writes from inside a read builtin)
  defeats dry-run. The stdlib stays on the right side of this line; custom code
  is your responsibility.

### The SIM: reads stay consistent with stubbed writes

The hard part of a useful dry-run is *read-after-write consistency*. A real
deploy does things like: start a new container, then check it's running, then
check it's healthy, then flip the proxy. If dry-run stubbed the "start" but then
ran a real "is it running?" check, the check would say *no* (because nothing
actually started) and the plan would diverge from reality — taking rollback
branches a real run never would.

`SimState` (`src/engine/sim.rs`) is the fix: a single in-memory model of "what
the hosts look like" for the duration of the dry run. The stdlib never inlines a
raw `docker inspect` or `nc -z` over `ssh_exec`; it reads and mutates this model
through typed `sim_*` builtins. So:

- `sim_docker_run(host, tag, name, cmd)` records the planned `cmd` **and** calls
  `set_running(host, name, tag)` in the sim, marking that container running and
  healthy.
- A subsequent `sim_container_running(host, name)` reads `true` from the sim.
- `sim_container_healthy(host, name)` reads `true` (the sim treats a
  freshly-started container as running *and* healthy).

The dry-run therefore takes the *same branches* a real run would.

**Reads are seeded lazily from exactly one real probe** per `(host, name)`
entity, on first access, then never re-read — they only change via a stubbed
mutation. From `sim_container_running`:

```rust
if snap.sim.lock().unwrap().is_seeded(host, name) {
    return snap.sim.lock().unwrap().is_running(host, name);
}
let real = real_inspect_running(&snap.runner, host, name);
let mut sim = snap.sim.lock().unwrap();
sim.seed_running(host, name, real)
```

That first probe is a *real* `docker inspect` over SSH. **Dry-run is not fully
offline:** it performs read-only probes (one per entity, plus `ssh_probe` calls)
to seed the model from current reality. It just never *mutates* anything.

Other sim conveniences and their limits:

- `sim_pick_port(host, base)` returns a **deterministic symbolic** port in
  dry-run: `base + 10000`, incrementing by one per pick per host. It does **not**
  probe — so repeated dry-runs print identical plans. In live mode it does a real
  `nc -z` scan for the first free slot. The dry-run port is therefore a
  placeholder, not necessarily the port a live run will choose.
- `sim_image_id(host, tag)` seeds from a real `docker image inspect` once; if the
  image isn't present it falls back to a branch-stable synthetic token `<tag>`
  (not a real digest).
- `http_get` / `http_post` short-circuit to a synthetic `200` and record a
  `check` line. This is deliberate: a `wait_healthy` loop against a
  not-yet-started container would otherwise fail or hang the plan. The flip side:
  **dry-run cannot tell you a health check would fail** — it assumes healthy.
- The `ContainerSim.image` field is "best-effort; not a real digest," and the
  seeded `health_ok` is derived from running state, not a real healthcheck.

The sim models containers, port occupancy, and proxy targets. It does **not**
model arbitrary filesystem state, database contents, DNS, or anything you reach
through a raw `ssh_exec`. Dry-run accuracy is exactly as good as how much of your
logic flows through `sim_*` / state builtins.

### The plan log

Recorded actions accumulate in `RunCtx.plan`. After a dry-run, `execute()`
prints them via `render_plan` (`src/engine/plan.rs`):

```
PLAN (dry run — no changes made):
  ssh     web1                   docker pull ghcr.io/app:v42
  ssh     web1                   docker run ... app-new
  check   web1                   pick free port from 13000
  write   web1                   write 128 bytes -> /run/app.env
  state   -                      app.version = <3 bytes>
  rollback -                     register compensation
  5 action(s), 1 host(s). 0 executed.
```

Each `PlannedAction` carries a `kind` tag (`local`, `ssh`, `ssh-all`,
`ssh-stdin`, `local-stdin`, `write`, `state`, `check`, `rollback`), an optional
`host`, and a human-readable `detail`. The trailing line always ends in
`0 executed.` — a dry-run executes no mutations by construction.

**Plan details are redacted at the recording boundary.** `RunCtx::record`
(`src/engine/context.rs`) runs every detail through secret redaction *before*
pushing it:

```rust
pub fn record(&self, kind: &str, host: Option<&str>, detail: String) {
    let detail = crate::engine::secret::redact(&detail, &self.secrets.lock().unwrap());
    ...
}
```

This matters because the plan prints to **stdout**, which bypasses the
`on_print` redaction hook (that hook only covers `print`/`debug`). Redacting in
`record` is the one boundary that keeps a `reveal()`'d secret out of the printed
plan.

Redaction is **substring-based**, though, so it only catches a secret that
reaches the detail *verbatim* — a value derived from one (a percent-encoded
password inside a `DATABASE_URL`, say) no longer contains the registered
plaintext. Two things cover that gap:

- **`state_set` never records the value.** Its detail is `key = <N bytes>` —
  the key and the value's size only — the same shape `write_remote` already uses
  for its body. `deploy()` persists its whole effective config through
  `state_set(service + ".config", to_json(cfg))`, so this is the one plan entry
  that would otherwise carry an arbitrary, secret-bearing blob to stdout.
- **`url_encode` registers the encoded form** of any registered secret it is
  handed, so `redact()` still matches the transformed value everywhere else
  (traces, `print`, errors). Only the encoding of an already-registered secret
  is registered — encoding an ordinary value never makes it redactable.

### No lock, no writes

`wire_run(dry_run)` in `src/cli/exec.rs` makes the no-side-effects guarantee
structural, not advisory:

- **No lock.** `if dry_run { HeldLock(None) }` — dry-run never opens or takes the
  advisory state lock, so it can never block (or be blocked by) a live run.
- **No state writes.** The store is loaded as an **overlay**:
  `StateStore::load_overlay(&root)`. An overlay reads the on-disk data into
  memory but has `root = None`, so its `flush()` is a no-op. `state_set` /
  `state_del` mutate the in-memory copy (keeping subsequent `state_get`s
  consistent within the run) and never touch disk.

So a dry-run is safe to run concurrently with anything, and leaves
`.energize/state.json` byte-for-byte unchanged.

---

## 2. State locking

Deploy history lives in `<project-root>/.energize/state.json`: a small key/value
map (`app.version`, `app.image`, etc.). Two protections guard it — a *lock* so
runs serialize, and an *atomic write* so a crash can't corrupt it.

### Project-root discovery

State is anchored to a project root, found by `find_project_root()`
(`src/engine/state.rs`) walking **up** from the current directory looking for one
of these markers:

```rust
const ROOT_MARKERS: &[&str] = &[".energize", "energize.toml", ".nrg-key"];
```

Four deliberate rules:

- **`.git` is *not* a marker.** This is intentional — `nrg` will not plant
  deploy state at an unrelated VCS root just because one happens to be above you.
- **The search is bounded by `$HOME`.** It never walks above your home
  directory.
- **`$HOME` itself is refused as a markerless root.** If the upward walk would
  land on `$HOME` with no marker present, `find_project_root` errors out rather
  than scaffolding `$HOME/.energize` — so a throwaway script run from your home
  directory can't silently create project state there. The error tells you to
  `cd` into a project or create an `energize.toml` / `.energize/`.
- **The directory a marker is accepted in must be one you control.** See below.

If no marker is found (and you're not at a bare `$HOME`), it defaults to the
current directory — safe first-run behavior: state is created where you invoked
`nrg`, not somewhere up the tree.

#### The root you adopt must be yours

The `$HOME` bound only stops a walk that is actually *inside* `$HOME`. Started
from `/tmp/...`, `/srv`, `/opt`, a CI workspace or a container `WORKDIR`, the
walk pops all the way to `/` — and it used to adopt the very first
marker-bearing directory it met on the way. Any other local user could therefore
drop a `.energize/` (or `energize.toml`, or `.nrg-key`) plus an `Energize.rhai`
into a world-writable ancestor and have `nrg` run it **as you**, with
`local_exec`, `ssh_exec` to your fleet and `secret()` all available to it. The
same root also supplies `<root>/.energize/secrets[.<dest>]` and `<root>/.env`
(whose `CMD[...]` values are handed to `sh -c`, `--dry-run` included) and the
state and audit files.

So the directory the marker is **accepted** in must be one the invoking user
controls — the same rule the key search applies (see
[Key discovery is bounded by what you own](#key-discovery-is-bounded-by-what-you-own),
and `src/trust.rs`, which both share):

- it must be **owned by the uid running `nrg`**, and
- it must **not be writable by other users**.

*Group*-writable is deliberately fine — `0775` roots and `0664` secrets files are
ordinary umask-002 defaults, not evidence of tampering. The sticky bit is not an
exemption (`/tmp` is `1777`: sticky stops other users deleting *your* entries, not
creating their own). Only the accepted directory is checked, **not** every
ancestor merely walked through — so a `0755` checkout under a `1777` parent keeps
working — and **not** the markerless current-directory fallback, which is just
where you invoked `nrg`, with nothing planted to lure you there.

A marker directory that fails the check is **refused**, loudly, naming the
directory and the reason, and `nrg` never quietly falls back to some other
candidate root. Secrets files get the same treatment: a `<root>/.energize/secrets`,
`secrets.<dest>` or `.env` that another uid owns or that is world-writable is
refused *when it is the file that defines the secret being asked for* — so a
stray file elsewhere in the search order changes nothing about secrets it never
mentions, but the moment one of them tries to supply a value, `secret()` throws
instead of using it.

Two consequences worth naming, the same two the key search has: a project root
that lives in a world-writable directory (a `0777` shared build area, say) is now
refused rather than used, and running `nrg` as a *different* user than the one
owning the project (`sudo nrg …` against your own checkout) is refused too —
root is not exempt from the ownership rule.

### Atomic, crash-safe writes

Every `set` / `del` persists the whole map atomically (`StateStore::flush`):

1. Back up the current `state.json` to `state.json.bak` (best-effort).
2. Write the new content to `state.json.tmp`.
3. `fsync` the temp file (`f.sync_all()`).
4. `rename` the temp file over `state.json` — atomic on POSIX.
5. On Unix, `fsync` the containing directory so the rename itself survives a hard
   crash (best-effort).

Because the publish step is an atomic rename of a fully-fsynced file, a crash
mid-write leaves a partial file in `state.json.tmp`, never a torn `state.json`.
That's also why there's no separate checksum — a partial write simply never
becomes the live file.

There's a second subtlety for concurrency-via-nesting. Before each mutation,
`set`/`del` call `reload_from_disk()` to re-read the on-disk map, *then* apply
the change, *then* flush the whole map. This prevents a stale in-memory copy from
clobbering keys written by a nested `nrg` invocation between your load and your
write (the map is written whole, so a blind flush would otherwise drop the
nested writer's keys).

### Corruption is fatal — by design

`StateStore::load` distinguishes three cases:

- **Missing file** → empty store. A legitimate first run.
- **Present but unparseable** → **fatal error**, refusing to run. This replaced
  an old `unwrap_or_default()` that would have silently reset deploy history. The
  error names the file and points at the backup:

  ```
  CORRUPT state file .../state.json (...). Refusing to run to avoid losing
  deploy history — inspect or restore it (a backup may exist at
  .../state.json.bak). Once fixed, re-run.
  ```

- **Future schema version** → fatal error, refusing to downgrade-rewrite it.

### Versioned schema

The on-disk file is a versioned wrapper:

```rust
struct StateFile { version: u32, data: BTreeMap<String, String> }
```

The current `STATE_VERSION` is `1`. A file whose `version` is **greater** than
this `nrg` understands is rejected ("Upgrade nrg to read it") rather than being
rewritten at the older version, which would silently strip fields a newer `nrg`
wrote. Equal-or-lower versions load.

### Advisory flock + re-entrancy

A live run takes an **exclusive advisory lock** on
`<root>/.energize/state.lock` via `fd_lock` (`flock`-style). The lock is held for
the *entire* run, not just during writes, so two concurrent live runs against the
same project serialize. `wire_run` first tries a non-blocking acquire so it can
print a friendly message before blocking:

```
Waiting for the state lock (another `nrg` run is in progress under <root>)...
```

By default the wait is indefinite. Pass `--lock-timeout <seconds>` (`nrg
exec`/`nrg run`) to give up after that many seconds instead, surfacing
`timed out after Ns waiting for the state lock under <root> — another nrg
run appears to be holding it` rather than hanging forever — useful in CI,
where a wedged or crashed prior run should fail the job quickly instead of
hanging until the runner's own timeout kills it uninformatively (robustness
review: "Blocking lock wait has no timeout").

The guard is leaked so it can live `'static` (released when the process exits).

**Re-entrancy** handles the nested case: a deploy hook that itself runs `nrg`.
When a run acquires the lock it sets `NRG_STATE_LOCK` to
`"<canonical-root>#<pid>"` — the symlink-resolved root path **plus this
process's own PID**. A nested invocation checks that env var against the root
it's about to lock AND verifies the recorded PID is still a live process
(`lock_is_reentrant`); only then does it **skip taking the lock**, reusing the
ancestor's, to avoid self-deadlock. Because state mutations re-read-then-write,
the nested writes still merge correctly rather than clobbering. The PID check
exists specifically so a *leaked* env var (one that names the right root but
whose process has long since exited — see "Limits" below) is never mistaken
for a live ancestor.

The liveness check (`pid_is_alive`) is `/proc/<pid>` existence on Linux, and
`kill -0 <pid>` (best-effort, no new dependency) elsewhere. This distinction
matters: `kill -0` fails with a nonzero exit for BOTH "no such process" AND
"process exists but is owned by a *different user*" — indistinguishable by
exit code alone. A nested `nrg` spawned under a different UID than its live
ancestor (e.g. a `pre_deploy_cmd` hook running `sudo -u deploy nrg ...`) would
otherwise wrongly see its own, still-running parent reported as dead and
deadlock on the flock the parent still holds. `/proc/<pid>` existence is
permission-proof — any user can `stat` another user's `/proc/<pid>` directory
to learn the process exists, even without permission to signal it — so the
Linux fast path doesn't have this gap **under the default procfs mount
options**. Non-Linux Unix targets (and Linux with no procfs mounted, e.g. some
minimal chroots/containers) fall back to `kill -0` and retain the EPERM
ambiguity as a known, documented limitation. A hardened host mounted with
`hidepid=2` (or `=1`) is a partial exception even on Linux: that option makes
`/proc/<pid>` invisible to a user who isn't its owner (or root), so a
cross-user nested invocation on such a host hits the same false-dead gap
`kill -0` has — a narrow, deliberately-hardened-host caveat, not a bug in the
fast path itself.

Limits worth knowing:

- The lock is **advisory** and **per-project-root**. It protects against other
  `nrg` runs, not against someone editing `state.json` by hand or a different
  tool writing the same hosts.
- Re-entrancy is keyed on the canonical root path via `NRG_STATE_LOCK`. A nested
  invocation targeting a *different* root takes its own lock normally.
- **The PID-liveness check is best-effort, not airtight** (robustness review:
  "Stale `NRG_STATE_LOCK` defeats serialization"). It closes the common case —
  an env var leaked across otherwise-unrelated invocations (e.g. a CI runner
  that doesn't reset its environment between job steps) whose original process
  has since exited. It does NOT close every case: the OS recycles PIDs, so an
  especially stale leaked value could in principle name a brand-new, unrelated
  process that happens to have reused the same PID, and would then be
  (incorrectly) treated as a live ancestor. A fully robust fix would need to
  verify process *ancestry*, not just liveness — there is no portable,
  dependency-free way to do that across Linux/macOS, so this is a deliberate,
  documented tradeoff rather than a gap nobody noticed.
- It's **local-machine-only**: two teammates (or a laptop plus a CI runner)
  deploying the same service from *different* machines each take their own,
  independent local lock and race freely against each other on the REMOTE
  hosts. See the cross-machine lock below for that gap (robustness review R15).

### Cross-machine deploy lock (robustness review R15)

`deploy()`'s per-host rolling swap (port pick, the canonical rename dance,
`<service>.target.<host>` state) has no protection against a SECOND,
*concurrent* deploy of the same service from a **different control machine** —
the local flock above only serializes runs on one machine. Two racing deploys
can both "successfully" pick the same free host port (a late failure then
unwinds an otherwise-healthy fleet) or interleave the rename dance so
`<service>-web`/`<service>-web-old` end up pointing at the wrong generation,
corrupting every subsequent deploy's rollback data.

`deploy()` closes this with a remote lock: an atomic `mkdir
/tmp/nrg-deploy-lock-<service>` on the FIRST host in the `hosts` array,
acquired before any build/push/pull/roll work and released (`rm -rf`) once the
whole deploy finishes, success or failure. `mkdir` IS the atomic
exclusive-create primitive here — no separate compare-and-swap needed: it
either creates the directory (lock acquired) or fails with "File exists"
(already held by someone else), distinguished from an unrelated SSH/mkdir
failure the same way this codebase's other remote-command classifiers work.
This is deterministic ONLY when concurrent callers agree on host order — two
racing deploys of "the same service" from a config/recipe both callers share
(the target scenario: a teammate's laptop and CI both running the checked-in
`Energize.rhai`) always pass the same `hosts` array in the same order, so they
pick the same lock host. A caller that reorders `hosts` between invocations
would defeat this — not something this lock tries to solve, just a boundary
worth knowing. On by default; opt out per-call with `cfg.skip_lock: true` if
needed (the lock depends on remote infrastructure — a writable `/tmp`, a
POSIX shell — this
tool can't unconditionally guarantee for every exotic host, unlike the
pure-Rhai R21/R29 guards, which have no escape hatch).

`rollback()` is covered automatically: it calls `deploy()` internally, so the
same lock protects a rollback-triggered redeploy too.

Limits, matching the local flock's own stance above:

- **No automatic staleness/TTL.** A deploy that crashes the control process
  outright, or one interrupted by SIGINT/SIGTERM — which this engine's R7
  interrupt handling deliberately makes an `ErrorTerminated` that BYPASSES a
  script-level `try`/`catch` (the exact reason `transaction()`'s own unwind
  relies on a Rust-level mechanism rather than Rhai `catch`) — leaves the lock
  held until an operator manually removes it (the refusal error names the
  exact `ssh <host> rm -rf <path>` command). A timeout short enough to matter
  risks letting two deploys run concurrently anyway on a slow-but-healthy one,
  which is worse than an occasional manual cleanup.
- No `nrg lock acquire/release/status` CLI surface for manual control (the
  Kamal model, tracked in `docs/roadmap.md`) — only the automatic
  acquire-then-release wired into `deploy()`/`rollback()` themselves.

### Deploy state may contain secret plaintext (robustness review R24)

Every successful `deploy()` persists the full effective config —
`state_set(service + ".config", to_json(effective_cfg))` — to
`<root>/.energize/state.json`. If `cfg.envs` was built from `reveal(secret(...))`
(the normal way to pass a secret into a container's environment, since Rhai's
`Secret` type can't be concatenated into a string), **the resolved plaintext
value is what gets persisted**, not the `Secret` wrapper.

This is a deliberate design tradeoff, not an oversight: `rollback()` reads this
exact key back (`replay = from_json(state_get(service + ".config"))`) to replay
the SAME env vars into a real redeploy. Redacting secret values out of the
persisted config — or refusing to persist them at all — would silently break
rollback for any service with a secret-bearing `cfg.envs`, restoring a
container missing (or with a garbled) credential instead of the working one
that predates the deploy that needs rolling back.

What actually protects it:

- `state.json` (and its `.bak`) are written **0600**, owner-only
  (`StateStore::flush`, `set_owner_only`) — verified by
  `state_file_is_written_0600` in `src/engine/state.rs`.
- That mitigates **local** exposure (another local user on the same box can't
  read it) but nothing more.

What it does **not** protect against, and what you must do yourself:

- **Never** commit `.energize/` to version control. This repo's own
  `.gitignore` deliberately excludes it — make sure yours does too.
- **Never** upload `.energize/` as a CI artifact, or bundle it into a workspace
  archive/tarball, without treating that artifact as equally sensitive as the
  secrets themselves.
- If you back up `state.json` (e.g. before a manual edit), treat the backup with
  the same care — copy it somewhere at least as access-restricted, and delete
  it once you're done.

If a service's secret needs of `cfg.envs` genuinely can't tolerate ever being
written to a local file (even 0600), don't route it through `deploy()`'s
`envs` — instead re-fetch it at container-start time from inside the
container itself (a secrets manager, a mounted volume, an init script that
calls `secret()`-equivalent tooling in-container), keeping the resolved
plaintext off the control machine's disk entirely. `nrg`'s stdlib does not
currently provide a built-in for that pattern.

---

## 3. Secrets

Secrets are loaded with `secret("NAME")`, which returns a tagged `Secret` value
(`src/engine/secret.rs`). The design goal: make every exposure of plaintext
*explicit and auditable*, and redact as a backstop everywhere else.

```rhai
import "lib/registry" as registry;

let pw = secret("REGISTRY_PASSWORD");   // a Secret, not a String
// registry_login(host, server, username, password) — host first; pw streams to
// `--password-stdin`, off-argv:
registry::registry_login("web1", "ghcr.io", "user", pw);
```

`secret(name)` looks up, in order:

1. `$NRG_SECRET_<UPPERCASE_NAME>` environment variable
2. `.energize/secrets.<dest>` — only when `--dest <dest>` is active
3. `.energize/secrets` (`KEY=VALUE`, optional surrounding quotes)
4. `.env` (same format)

Whichever source produces the raw value, two special framings are then applied: a
`CMD[command]`-framed value runs `command` locally and uses its stdout as the real value (the
Kamal-style fetch-adapter integration point for 1Password/Bitwarden/Vault/Doppler/etc — see
[Builtins reference](builtins.md#secretname---secret)), and an `ENC[...]`-framed value is
decrypted via the discovered `.nrg-key`. `CMD[...]` is real local shell execution — the same
trust level as writing any other line in your own `.energize/secrets`/`.env`, but worth naming
explicitly: whoever can set `$NRG_SECRET_<NAME>` or edit either file gets local command
execution, not just a bad secret value. It also runs even under `--dry-run` (like `ENC[...]`
decryption), so a dry run can invoke your secret-manager CLI and requires you already be
authenticated to it.

It **throws** if the secret is missing, if a `CMD[...]` fetch command fails, or if the final
value is shorter than `MIN_SECRET_LEN` (**6** characters) — see below.

### Key discovery is bounded by what you own

The `.nrg-key` / `.nrg-key.pub` used by `secret()`'s `ENC[...]` decryption and by
every `nrg secrets` subcommand is found by walking **up** from the current
directory. That walk has two boundaries, and whatever it finds still has to earn
trust:

- **`$HOME`** — it never searches above your home directory. But `$HOME` only
  bounds a walk that is actually *inside* `$HOME`: run from `/tmp/...`, `/srv`,
  `/opt`, a CI workspace or a container `WORKDIR`, that test never fires.
- **Ownership** — so the walk also stops as soon as it reaches a directory the
  invoking user does not control (one another uid owns, or one that is
  world-writable), rather than popping all the way to `/`. With no home
  directory at all (`dirs::home_dir()` returning `None`) only the starting
  directory is searched.
- **The key file itself is vetted**: it must be owned by the uid running `nrg`
  and must not be world-writable, and neither must the directory holding it.
  Symlinks are judged both by the link's own ownership and by what it resolves
  to. *Group*-writable files and directories are deliberately fine — `0664` /
  `0775` are ordinary umask-002 defaults, not evidence of tampering.

A key file that fails those checks is **refused**, loudly, naming the file and
the reason — `nrg` never quietly falls back to a different key, because quietly
using a different key is the whole problem: a `.nrg-key.pub` planted by any other
local user in a world-writable ancestor would otherwise become the recipient
every `ENC[...]` token and `.enc` file is encrypted to, readable by whoever holds
the matching private key. `nrg secrets encrypt` / `seal` also print the public
key they resolved (on stderr), and re-validate that it really is an `age1…`
recipient before encrypting to it.

Two consequences worth naming: running `nrg` as a *different* user than the one
owning the project (`sudo nrg …` against your own checkout, say) is refused
rather than silently trusted — root is not exempt from the ownership check — and
a key that lives above a world-writable directory is no longer discovered from
below.

### The key does not ride along to a build host

Vetting where the key comes *from* is only half of it: the key also has to stay
where it is. `docker_build`'s [`cfg.build_host`](deploy.md#multi-arch-builds)
copies the whole build context to another machine over SSH, and a build context
is usually `"."` — the project root, which is exactly where `.nrg-key` lives.
A plain "archive the directory" sync would therefore hand the unpassphrased age
identity that decrypts *every* `ENC[...]` secret to a third machine, into a
world-listable `/tmp`, for a build that never needed it.

So the sync enumerates the context root itself and skips four entries by name:

- `.nrg-key` — the private age identity
- `.nrg-key.pub`
- `.energize/` — deploy state, which
  [may contain secret plaintext](#deploy-state-may-contain-secret-plaintext-robustness-review-r24)
- `.env` — a `secret()` lookup source

Only the **root** is skipped, so a nested `config/.env` that your image really
builds against is still sent; if a build genuinely needs one of those four
names, copy it to a different one inside the context. A context that holds
nothing *but* those four fails locally, with a message saying why, before
anything is sent.

Three supporting properties, since a sync is a copy of your source onto a
machine you may share:

- The remote directory is created `0700` in a single `mkdir -m 700`, not
  widened afterwards. Nothing in this path runs `chmod` on a `/tmp` path:
  `chmod` follows symlinks, so a local user on the build host who wins the race
  between the `rm -rf` and the `mkdir` could otherwise aim it at a directory of
  their choosing, with the SSH build user's privileges.
- The local temp archive is created under `umask 077`, so it isn't
  world-readable on the build machine either.
- The synced context is deleted as the last step of the same SSH command that
  runs the build — pass or fail — instead of sitting in `/tmp` until the next
  sync to the same tag. The build's own exit code, stdout and stderr are
  unchanged by that.

This is a defense against *accidental* spread, not a sandbox: `build_host` runs
a build you wrote, on a host you chose, with a Dockerfile that can read anything
you did send it.

### The tagged `Secret` type

`Secret` is deliberately *not* convertible to a `String` in scripts. The only
ways to reach the plaintext are two explicit functions:

```rust
// reveal(secret) -> String   (explicit un-wrap)
engine.register_fn("reveal", |s: Secret| -> String { s.reveal().to_string() });

// sh_quote(x) -> String   for both String and Secret (the only safe interpolation path)
engine.register_fn("sh_quote", |s: &str| -> String { posix_quote(s) });
engine.register_fn("sh_quote", |s: Secret| -> String { posix_quote(s.reveal()) });
```

Everything else that could leak plaintext is blocked or neutered:

- **No string concatenation.** Rhai would otherwise auto-stringify a `Secret` via
  `to_string()` and silently build a broken command. Instead, `+` with a `Secret`
  on either side is registered to **throw**:

  ```
  refusing to concatenate a Secret into a string; use sh_quote(secret) for a
  shell argument or reveal(secret) for explicit plaintext
  ```

  This applies to `str + Secret`, `Secret + str`, and `Secret + Secret`.

- **`to_string()` / `to_debug()` return `"***"`**, not the value. So
  `print(my_secret)` prints `***`.

- **Rust `Debug` can't leak it either.** A hand-written `impl Debug` prints
  `Secret(***)`, so the plaintext can't surface through an error message or a
  container like `[Secret(***)]`. (A derived `Debug` would have printed
  `Secret("plaintext")`.)

- **There is no `state_set(key, secret)` path.** `state_set` takes a `&str`, and
  you can't coerce a `Secret` to a string — so a secret can't be persisted to
  `state.json` by accident. (If you `reveal()` it first and store the string,
  that's an explicit choice on you.)

So in practice you do one of two things with a `Secret`:

```rhai
let pw = secret("REGISTRY_PASSWORD");

// Shell argument — POSIX-quoted, safe against injection:
let cmd = "some-tool --token " + sh_quote(pw);
local_exec(cmd);

// Off-argv plaintext for a stdin channel:
ssh_exec_stdin(host, "tool login --password-stdin", reveal(pw));
```

### `--password-stdin`: keep plaintext off the argv

Process arguments are visible in `ps` and in shell history; stdin is not. The
exec builtins provide a stdin channel for exactly this:

- `ssh_exec_stdin(host, cmd, stdin)` and `local_exec_stdin(cmd, stdin)` deliver
  the payload **off-argv** — it is never placed on the command line and never
  traced.
- `write_remote(host, content, remote_path)` writes `content` to a `0600` remote
  file (`umask 077; cat > '<path>'`) via the same stdin channel — for secret
  env-files and configs.

The stdlib's `registry::registry_login` uses this: the password streams to
`<container-cmd> login ... --password-stdin` through `local_exec_stdin` /
`ssh_exec_stdin` with `reveal(password)` as the stdin argument, so the plaintext
appears only on stdin, only for that moment.

The trace output is careful here too: `ssh_exec_stdin` logs the command and the
stdin *byte count*, never the stdin content:

```
[nrg] ssh_exec_stdin web1 -> docker login -u u --password-stdin (stdin 11 bytes)
```

### SSH host-key verification: the transport under those secrets

Keeping a password off the argv is worth nothing if it is streamed to the wrong
machine. Every `ssh` invocation `nrg` builds (`RealRunner::ssh_command` in
`src/engine/runner.rs`, `ssh_stream_command` in `src/cli/logs.rs`) sets
`StrictHostKeyChecking=yes` by default: a host whose key is not already in
`known_hosts` is **refused**, and since `BatchMode=yes` is also set, nothing
prompts. Fail closed is the default because the same connection carries registry
passwords into `docker login --password-stdin` and plaintext env-files into
`write_remote`.

`$NRG_SSH_HOST_KEY_CHECKING` overrides it:

| Value | Meaning |
| --- | --- |
| `yes` (default) | Only hosts already in `known_hosts`; unknown host = connection refused. |
| `accept-new` | Trust-on-first-use — pin an unknown host's key on first contact. Convenient, but the first connection is unauthenticated: a machine-in-the-middle present at that moment gets the secrets and the pin. |
| `no` / `off` | No checking at all. Not recommended. |
| `ask` | Prompt — useless under `BatchMode=yes`; behaves as a refusal. |

An unrecognized value falls back to the default rather than being passed through,
so a typo can't silently weaken the policy.

**Pre-seeding `known_hosts` (CI, fresh containers).** A CI runner starts with an
empty `known_hosts`, so with the default it will refuse to connect until you
populate it — do that as a setup step, from a key you already trust:

```sh
mkdir -p ~/.ssh && chmod 700 ~/.ssh
# Keep the expected fingerprints in your CI secrets/config, not scraped at run time:
printf '%s\n' "$KNOWN_HOSTS" >> ~/.ssh/known_hosts
chmod 600 ~/.ssh/known_hosts
```

Generate the `$KNOWN_HOSTS` contents once, from a machine you trust, with
`ssh-keyscan -H <host>` — then verify the fingerprints against the host's own
(`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`, read over the provider
console) before storing them. `ssh-keyscan` run inside the CI job itself is just
trust-on-first-use spelled differently: it accepts whatever key answers, so it
gives you no more assurance than `accept-new`.

### Redaction as an output boundary

The `Secret` type is the *primary* guard; redaction is defense-in-depth applied
at each place text leaves the process:

- **`print` / `debug` output** is routed through `redact` via `on_print` /
  `on_debug` (`src/engine/mod.rs`), so even `print(reveal(s))` prints `***`.
- **Thrown errors** (which can carry secret-bearing command stderr) are redacted
  before printing — `run_file` / `run_fn` map errors through `redact`.
- **The dry-run plan log** is redacted at `RunCtx::record` (covered above),
  because it prints straight to stdout and skips `on_print`.
- **Command traces** (`NRG_TRACE`) are redacted via `traced()` before logging.

`redact` replaces every registered secret value (those resolved through
`secret()`) with `***`, longest-first for deterministic results when one secret
is a substring of another.

### `sh_quote` is POSIX single-quoting

`posix_quote` wraps a value in `'…'` and renders any embedded `'` as `'\''`:

```rust
pub fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
```

Everything inside stays literal — spaces, `$`, backticks, newlines — so a value
can't break out of its argument or inject a command. This is the **only** safe
way to interpolate a string (secret or not) into a shell command you build by
hand.

### Honest limits

- **Redaction is substring-based.** It can only blank a secret that appears
  *verbatim* in the output. A secret that was transformed before reaching the
  sink — base64-encoded, hashed, URL-escaped, split across lines — will **not**
  be caught by `redact`. This is an accepted tradeoff; the `Secret` type, not
  redaction, is the real guard.
- **`MIN_SECRET_LEN` is 6.** Two reasons: a very short secret is weak anyway, and
  substring redaction of a 1–3 character "secret" would blank ordinary output
  (e.g. redacting `"ab"` would mangle `"abacus"`). `redact` ignores registered
  values shorter than this, and `secret()` refuses to load one.
- **`reveal()` is a real escape hatch.** Once you `reveal()` a secret into a
  plain `String`, the type system stops helping you — that string can be
  concatenated and stored. Redaction still covers output sinks, but storage and
  transformation are on you. Prefer `sh_quote(secret)` and the `*_stdin`
  builtins over `reveal()` where possible.

### Escape hatches: trusted-input-only raw shell (robustness review R28)

Every interpolated value this stdlib builds into a remote/local shell command
is `sh_quote()`'d — **except four fields, which are spliced in VERBATIM, with
NO quoting or escaping applied at all**:

| Field | Where |
| --- | --- |
| `cfg.extra` | `docker_run` (`lib/docker.rhai`) — appended raw to the `docker run` command line |
| `command` | `docker_run_once(host, tag, command, cfg)` (`lib/docker.rhai`) — the container's entrypoint command |
| `command` | `docker_exec(host, name, command)` (`lib/docker.rhai`) — the command run inside a live container |
| `cfg.pre_deploy_cmd` / `cfg.post_deploy_cmd` | `deploy()` (`lib/deploy.rhai`) — raw per-host hook commands run via `ssh_exec` |

These are **intentional escape hatches**, not gaps: they exist so you can pass
a real shell command (`"bin/rails db:migrate"`, `"systemctl restart nginx"`)
without this stdlib trying to guess how to quote an entire command line for
you. But that also means the safety contract the REST of the stdlib gives you
— "every value you hand us is safely quoted, so it can't break out of its
argument position" — **does not apply to these four fields**. A reader who
assumes "everything nrg touches is quoted" and passes one of these fields
untrusted input (e.g. a value derived from a webhook payload, a PR title, or
any other externally-controlled string) has built a shell-injection
vulnerability, not used a safety feature nrg forgot.

Rules for these four fields:

- **Build them only from strings YOU wrote** (literals in your
  `Energize.rhai`, or values from your own trusted config — never from
  external/user-controlled input).
- **Keep secrets out of them.** They're commands, not data — pass secrets via
  `cfg.envs` (delivered through the 0600 env-file mechanism, itself validated
  against embedded newlines, robustness review R19) or `ssh_exec_stdin`/
  `write_remote`'s off-argv stdin channel instead.
- If you need to embed a caller-supplied VALUE inside one of these raw
  commands (not just static text), `sh_quote()` that value yourself before
  splicing it in — the same way this stdlib does internally for every other
  field.

---

## 4. Transactions

A deploy is a sequence of mutations. If step 4 throws, steps 1–3 already
happened. `transaction()` / `on_rollback()` (`src/engine/transaction.rs`) give
you a compensation stack to undo completed work in reverse.

```rhai
import "lib/docker" as docker;
import "lib/proxy" as proxy;
import "lib/healthcheck" as health;

// `host`, `image`, and `old_target` are bound earlier in the deploy.
transaction(|| {
    docker::docker_run(host, image, "app-new", #{ ports: #{ "3001": "3000" } });
    on_rollback(|| { docker::docker_remove(host, "app-new"); });   // undo: remove the new container

    health::wait_healthy_on_host(host, 3001, #{});                 // throws if unhealthy (checks
                                                                    // ON `host` over SSH — R7-health)

    on_rollback(|| { proxy::proxy_deploy(host, "app", old_target); }); // undo: restore the proxy
    proxy::proxy_deploy(host, "app", "localhost:3001");            // flip traffic to the new container
    // if anything below throws, both on_rollback closures run, newest-first
});
```

### Register-before-effect is the contract

`on_rollback(cb)` pushes a compensation closure onto the stack. The intended
pattern is to register the undo **immediately after** (or, defensively, before)
the effect it compensates. Only compensations registered *during* a transaction
body, and registered before the throw, are on the stack to be unwound — so if you
register *after* the effect and the effect throws first, that effect won't be
compensated. Pairing each effect with its `on_rollback` tightly is the discipline
the API assumes.

### LIFO, best-effort, error-isolated unwind

When the `transaction` body returns `Err` (an uncaught `throw`), the runtime
drains the compensations registered during it, **last registered first**, then
**re-raises the original error**. The unwind is:

- **LIFO** — newest compensation runs first, mirroring how you'd manually undo.
  In the test, body `do-1, do-2` then throw yields `undo-2, undo-1`.
- **Best-effort and error-isolated** — if a compensation *itself* throws, the
  unwind logs and **continues** with the remaining ones rather than aborting:

  ```rust
  if let Err(ce) = c.call_within_context::<()>(&context, ()) {
      eprintln!("[nrg] rollback step failed (continuing): {ce}");
  }
  ```

  So one failed undo doesn't strand the rest. (It is logged to stderr, not
  re-thrown.)

- **Re-raising** — after unwinding, the *original* failure propagates. The
  transaction doesn't swallow your error; it cleans up and re-throws so the run
  still exits non-zero (unless you `catch` it yourself).

The unwind pops **one** compensation under a short lock and releases that lock
*before* invoking it. Two properties fall out of this:

- A compensation can safely call back into `nrg` builtins (`local_exec`, etc.)
  that lock the context — the unwind isn't holding the context or txn lock across
  the call, so there's no deadlock.
- A compensation that registers *another* `on_rollback` during unwind pushes onto
  the live stack and the next pop picks it up — nothing is lost or leaked.

### Nesting

Transactions track a nesting `depth`, queryable from a script via
`in_transaction()` (true whenever `depth > 0`). On a **nested** success, the
inner transaction keeps its compensations on the stack so an enclosing
transaction's failure still unwinds them (the stacks flatten). Only the
**outermost** commit (`depth == 0`) truncates the stack back to its starting
mark and drops the compensations. Sequential (non-nested) transactions don't
cross-unwind: a committed transaction's compensations are gone before the
next one runs.

**Caution if you build your own transaction-wrapping stdlib function**: this
flatten behavior means a function that runs its own `transaction()` and then
does further, non-compensated work right after it returns `Ok` is only safe
to call at the top level. If nested inside a caller's transaction, that
"commit" isn't final — an unrelated later failure in the outer transaction
can resurrect the inner function's already-superseded compensations against
state your later work already changed. `lib/deploy.rhai`'s `deploy()` (and
`rollback()`, which calls it internally but has its own earlier state-mutating
side effect) hit exactly this (robustness review R29) and both now call
`in_transaction()` as their first statement, refusing to run (rather than
risk it) when already nested.

### Dry-run records, never invokes

In dry-run, `on_rollback` does **not** push a real closure — it records a
`rollback` `PlannedAction` ("register compensation") and returns. Nothing is ever
invoked:

```rust
if mode == EffectMode::DryRun {
    ctx.lock().unwrap().record("rollback", None, "register compensation".into());
} else {
    ctx.lock().unwrap().txn.lock().unwrap().comps.push(cb);
}
```

So a dry-run shows you *that* compensations would be registered (and how many),
without running any undo logic. Note that `transaction()` itself still runs its
body in dry-run — the body's mutating builtins record plan entries as usual — but
because the stack stays empty, a `throw` inside a dry-run transaction won't
trigger any (no-op) unwind.

### Ctrl-C (SIGINT/SIGTERM) triggers the same unwind

`nrg` installs a SIGINT/SIGTERM handler once per `nrg exec`/`nrg run`
invocation — live or dry-run; harmless either way, since dry-run has nothing
real to unwind (`engine::interrupt::install`) — that flips a shared flag. The
engine polls that flag between every script-level operation
(`Engine::on_progress`); when set, it ends the running script with a normal
`Err` — the exact path an uncaught `throw` takes — so an enclosing
`transaction()` unwinds exactly as described above, instead of Ctrl-C killing
the process outright with zero cleanup. The state lock then releases via its
normal `Drop` (`RunWiring::_lock` going out of scope), not because the OS
reclaimed the fd on process death.

The flag is **consumed** the moment it's checked (an atomic `swap`, not a
`load`): the interrupt both terminates whatever's currently running and clears
itself, so the `on_rollback` compensations that run during the unwind aren't
immediately re-terminated by the same still-set flag.

**Scope — what this can't preempt.** `on_progress` is checked *between*
operations, not *during* one blocking native call. A `for` loop (e.g.
`healthcheck.rhai`'s retry loop, bounded by a few seconds of `sleep()` per
iteration) responds within about one iteration — the realistic "stuck waiting
on a health check" case Ctrl-C is reached for. A single long- or
forever-blocking `ssh_exec`/`local_exec`/`http_get` call can't be interrupted
mid-flight; the check only fires once that call returns. `ssh_exec`'s
underlying `ssh` now sets a keep-alive (`ServerAliveInterval`/
`ServerAliveCountMax`, robustness review R5), so a connection that's gone
silently dead resolves on its own within about a minute rather than blocking
forever — but a remote command that's genuinely still running, just very
slow, has no wall-clock cap yet; that's a separate, still-open gap — see
[Robustness Review](robustness-review.md).

**Force-quit escape hatch.** Installing a handler for a signal replaces its
default "terminate immediately" behavior — so without a second tier, a signal
delivered while `nrg` is stuck inside one of the blocking calls above would
just set the flag and go unnoticed until that call eventually returns, leaving
the operator with no way to force-quit short of `SIGKILL`/`SIGQUIT`. A
**second** SIGINT/SIGTERM (received any time after the first already armed
the flag — including while still stuck in a blocking call) exits the process
immediately, no further cleanup, via `signal_hook`'s
`register_conditional_shutdown`. One signal tries to unwind gracefully; two
means "stop trying and just exit."

### Honest limits

- **Compensations are your code.** The runtime guarantees ordering (LIFO),
  isolation (one failure doesn't abort the rest), and re-raise. It does **not**
  guarantee your undo is correct, complete, or itself atomic. A rollback that
  partially fails leaves partial state — logged, but not retried.
- **Only registered, body-scoped compensations unwind.** An effect with no
  `on_rollback`, or one registered after the throwing call, is not undone.
- **Best-effort means best-effort.** If a rollback step throws, you get a stderr
  line and the unwind continues — there is no rollback-of-the-rollback. Design
  compensations to be idempotent and tolerant of "already undone" states.
- **Ctrl-C is checked between operations, not during one blocking call.** See
  "Ctrl-C (SIGINT/SIGTERM) triggers the same unwind" above for exactly what
  this can and can't preempt.

---

## Addendum: deployed containers publish on loopback

The four mechanisms above are about how `nrg` itself runs. This one is about
what a deploy *leaves behind* on the host.

`deploy()` starts each new app container with
`-p 127.0.0.1:<picked_port>:<container_port>`, so the auto-picked host port is
bound to **loopback** rather than every interface. Publishing it on `0.0.0.0`
would put the app on the network at a predictable port (the scan starts at
`container_port + 10000`) with no TLS, no `cfg.domain` host match and no
`proxy_maintenance` 503 — the proxy enforces all three, and a direct connection
to the container's own port skips it. A host firewall is not a substitute:
Docker's published-port DNAT rules are evaluated *before* the `INPUT` chain a
`ufw`/`firewalld` rule lives in.

Where the guarantee stops:

- **Only `deploy()`'s own container.** `accessory_run` publishes the `ports` map
  you give it verbatim (a cross-host `db_host` database depends on that), so a
  bare `#{ "5432": "5432" }` accessory is still on every interface. Write the
  bind into the key — `#{ "127.0.0.1:5432": "5432" }` — if you want otherwise.
- **`publish_all_interfaces: true` opts back out** of the loopback bind for the
  app container, deliberately restoring the exposure above.
- **It is a bind address, not authentication.** Anything already on the host —
  another container with host networking, any local user — still reaches the
  port. See [`docs/deploy.md`](deploy.md#published-ports-bind-to-loopback).
