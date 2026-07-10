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
  state   -                      app.version = v42
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

Three deliberate rules:

- **`.git` is *not* a marker.** This is intentional — `nrg` will not plant
  deploy state at an unrelated VCS root just because one happens to be above you.
- **The search is bounded by `$HOME`.** It never walks above your home
  directory.
- **`$HOME` itself is refused as a markerless root.** If the upward walk would
  land on `$HOME` with no marker present, `find_project_root` errors out rather
  than scaffolding `$HOME/.energize` — so a throwaway script run from your home
  directory can't silently create project state there. The error tells you to
  `cd` into a project or create an `energize.toml` / `.energize/`.

If no marker is found (and you're not at a bare `$HOME`), it defaults to the
current directory — safe first-run behavior: state is created where you invoked
`nrg`, not somewhere up the tree.

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

The guard is leaked so it can live `'static` (released when the process exits).

**Re-entrancy** handles the nested case: a deploy hook that itself runs `nrg`.
When a run acquires the lock it sets `NRG_STATE_LOCK` to the canonical
(symlink-resolved) root path. A nested invocation sees that env var matches the
root it's about to lock (`lock_is_reentrant`) and **skips taking the lock**,
reusing the ancestor's, to avoid self-deadlock. Because state mutations
re-read-then-write, the nested writes still merge correctly rather than
clobbering.

Limits worth knowing:

- The lock is **advisory** and **per-project-root**. It protects against other
  `nrg` runs, not against someone editing `state.json` by hand or a different
  tool writing the same hosts.
- Re-entrancy is keyed on the canonical root path via `NRG_STATE_LOCK`. A nested
  invocation targeting a *different* root takes its own lock normally.

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
2. `.energize/secrets` (`KEY=VALUE`, optional surrounding quotes)
3. `.env` (same format)

It **throws** if the secret is missing, and also throws if it is shorter than
`MIN_SECRET_LEN` (**6** characters) — see below.

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

    health::wait_healthy("http://" + host + ":3001/up", #{});      // throws if unhealthy

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

Transactions track a nesting `depth`. On a **nested** success, the inner
transaction keeps its compensations on the stack so an enclosing transaction's
failure still unwinds them (the stacks flatten). Only the **outermost** commit
(`depth == 0`) truncates the stack back to its starting mark and drops the
compensations. Sequential (non-nested) transactions don't cross-unwind: a
committed transaction's compensations are gone before the next one runs.

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

`nrg` installs a SIGINT/SIGTERM handler once per live run (`engine::interrupt::install`)
that flips a shared flag. The engine polls that flag between every script-level
operation (`Engine::on_progress`); when set, it ends the running script with a
normal `Err` — the exact path an uncaught `throw` takes — so an enclosing
`transaction()` unwinds exactly as described above, instead of Ctrl-C killing
the process outright with zero cleanup. The state lock then releases via its
normal `Drop` (`RunWiring::_lock` going out of scope), not because the OS
reclaimed the fd on process death.

The flag is **consumed** the moment it's checked (an atomic `swap`, not a
`load`): the interrupt both terminates whatever's currently running and clears
itself, so the `on_rollback` compensations that run during the unwind aren't
immediately re-terminated by the same still-set flag. A second Ctrl-C during
the unwind sets it again and is caught the same way — a determined double-
interrupt can still cut a compensation short, which is expected "force quit"
behavior, not a bug.

**Scope — what this can't preempt.** `on_progress` is checked *between*
operations, not *during* one blocking native call. A `for` loop (e.g.
`healthcheck.rhai`'s retry loop, bounded by a few seconds of `sleep()` per
iteration) responds within about one iteration — the realistic "stuck waiting
on a health check" case Ctrl-C is reached for. A single long- or
forever-blocking `ssh_exec`/`local_exec`/`http_get` call can't be interrupted
mid-flight; the check only fires once that call returns. A truly hung remote
command (no timeout, network black hole) is a separate, still-open gap — see
[Robustness Review](robustness-review.md).

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
