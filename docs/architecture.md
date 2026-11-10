---
title: Architecture
nav_order: 9
---

# Architecture (engine internals)

This is the contributor's map of the Energize (`nrg`) execution engine — the part of
`src/` that compiles a `.rhai` orchestration module and runs its builtins (`ssh_exec`,
`state_set`, `transaction`, …) against real hosts. If you are writing a builtin, adding a
dry-run path, or chasing a locking/redaction bug, start here.

It documents the code as it is, not as it might one day be. There is **no** nginx / TLS /
provisioning module in the engine. The stdlib drives one reverse proxy, Caddy (`std/caddy`),
through the ordinary `ssh_exec`
builtin (the engine itself knows nothing about proxies). The old Starlark
and bash runtimes have been removed; Rhai is the only scripting language.

---

## Module layout

Everything lives under `src/engine/`:

| Module | Responsibility |
| --- | --- |
| `mod.rs` | `build_engine()` — assembles a Rhai `Engine` with types, builtins, secrets, transactions, and stderr-redacting `print`/`debug`. |
| `eval.rs` | Compile a `.rhai` file with the module resolver anchored at its directory; `run_file` (exec) and `run_fn` (`nrg run <fn>`); `list_functions` (`nrg tasks`). |
| `context.rs` | `RunCtx` (the per-run shared state), `EffectMode`, `TxnState`, and the `SharedCtx = Arc<RunCtx>` handle threaded into every builtin. |
| `runner.rs` | `CommandRunner` trait + `RealRunner` (spawns `ssh`/`sh`) and `FakeRunner` (test double, `#[cfg(test)]`). |
| `types.rs` | `ExecResult` and `HttpResponse` — the read-only result types exposed to scripts. |
| `state.rs` | `StateStore`: project-anchored, lock-serialized, atomically-written `.energize/state.json`; plus the `fd_lock` advisory-lock helpers. |
| `secret.rs` | The `Secret` type, POSIX quoting, secret lookup, and the `redact()` substring scrubber. |
| `transaction.rs` | `transaction()` / `on_rollback()` — the compensation stack and LIFO unwind. |
| `plan.rs` | `PlannedAction` and `render_plan()` — the dry-run plan log. |
| `builtins/` | The builtins: `exec.rs` (command effects and the dry-run dispatch helpers), `transfer.rs`, `http.rs`, `probe.rs`, `state.rs`, `steps.rs`, `preflight.rs`, `util.rs`. |

The CLI entry wiring is in `src/cli/exec.rs`: `wire_run` (root discovery + lock + state + ctx)
is shared by `nrg exec` and `nrg run`. Each command has its own `execute` (`exec.rs` calls
`eval::run_file`; `run.rs` calls `eval::run_fn`).

`src/cli/rehearse.rs` has a separate declaration-only Rhai engine with bounded
evaluation and only the static `std/contracts`/`std/release_recipes` modules. It
registers no execution, HTTP, credential, or state builtins and no filesystem
module resolver. The returned map is validated completely before any live work.
Explicit local execution reuses `RealRunner::run_isolated_local` and the existing
concurrent, redacted pipe handling. It bypasses project discovery and `wire_run`,
uses an owned temporary directory and a cleared child environment, and returns
its own report. This isolates workspace/state conventions, not OS permissions.

---

## `build_engine()` — how the engine is wired

`build_engine(ctx: SharedCtx) -> Engine` (`src/engine/mod.rs`) is the single place every
piece is registered. In order:

```rust
let mut engine = Engine::new();
engine.set_max_operations(0);       // trusted scripts: unlimited ops
engine.set_max_expr_depths(0, 0);   // lift the 32-deep expr cap (stdlib builds long a+b+c chains)
engine.set_max_call_levels(64);     // lift the function-CALL-nesting cap too
                                     // (independent of the expr-depth limit above). Rhai
                                     // defaults it to just 8 in a debug build, deep enough for
                                     // deploy()'s own multi-module call chain to trip on a
                                     // realistic rollback() call. Raised to 64 (Rhai's own
                                     // release-build default), not higher — a higher cap lets
                                     // genuine infinite recursion hard-abort (SIGABRT) instead of
                                     // a clean catchable error on a 2 MiB thread stack

// print/debug routed to stderr THROUGH secret redaction:
engine.on_print(|s| eprintln!("{}", secret::redact(s, &secrets)));
engine.on_debug(|s, _, pos| eprintln!("[debug] {} @ {pos:?}", secret::redact(s, &secrets)));

types::register_types(&mut engine);            // ExecResult, HttpResponse
builtins::register_builtins(&mut engine, ctx); // exec, http, probe, state, util
secret::register(&mut engine, ctx);            // Secret type, secret(), reveal(), sh_quote()
transaction::register(&mut engine, ctx);       // transaction(), on_rollback()
```

Notes worth internalizing:

- The safety limits are lifted **on purpose**. Scripts are trusted (you wrote them). The
  stdlib assembles long `"docker run " + a + " " + b + …` command strings and deeply nested
  `if cfg.contains(k) { … } else { … }` config chains whose ASTs exceed Rhai's default
  function-body depth of 32, so the depth cap is removed too.
- `print`/`debug` go to **stderr**, not stdout, and are run through `secret::redact` first.
  This is defense-in-depth — the `Secret` type is the primary guard — but it means a script
  that prints a `reveal()`'d secret still can't leak it to the console.
- The **module resolver is NOT set here.** `build_engine` produces a resolver-less engine;
  `eval.rs` sets a `FileModuleResolver` anchored at the running file's directory afterward
  (see below). A plain `build_engine` engine can't resolve `import "std/x" as x;` (embedded)
  or a relative file import.

---

## `RunCtx` and the shared-handle pattern

`RunCtx` (`src/engine/context.rs`) is the per-`nrg`-invocation state. Builtins reach it
through `SharedCtx = Arc<RunCtx>`, a clone of which every builtin closure captures. There is
no outer lock: `mode` and `trace` are set once at construction and never mutated, and every
other field is either a `Mutex` or an `Arc<Mutex<…>>` with its own lock.

```rust
pub struct RunCtx {
    pub mode: EffectMode,                       // Live | DryRun
    pub steps: Mutex<Vec<StepRecord>>,          // diagnostics
    pub journal: Mutex<Option<Journal>>,
    pub remote_locks: Mutex<Vec<RemoteLock>>,
    pub runner: Arc<dyn CommandRunner>,         // its OWN Arc
    pub state: Arc<Mutex<StateStore>>,          // persistent state; its own lock
    pub session: Arc<Mutex<BTreeMap<String, String>>>, // in-memory only
    pub secrets: Arc<Mutex<HashSet<String>>>,   // plaintext secrets, for redaction
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,   // dry-run plan log
    pub txn: Arc<Mutex<TxnState>>,              // compensation stack
    pub trace: bool,                            // NRG_TRACE env var
    pub interrupted: Arc<AtomicBool>,           // set by the SIGINT/SIGTERM handler
    pub was_interrupted: AtomicBool,
}
```

A builtin reads `ctx.mode` and `ctx.trace` directly, and takes a short lock on a field only
for the moment it needs it. For slow work it clones the `Arc` it needs (`ctx.runner.clone()`)
and then blocks on that clone, holding no lock. Cloning an `Arc` bumps a refcount; it is the
**same** allocation, so mutations through the clone are visible to everyone else.

### Why this matters: parallel `ssh_exec_all`

The canonical example is `ssh_exec_all` (`src/engine/builtins/exec.rs`). It fans a command
out across many hosts **in parallel** using `std::thread::scope`, in chunks of 16:

```rust
let results = effect_fanout(&ctx, "ssh-all", host_strs, &cmd, |ctx, hosts| {
    let runner = ctx.runner.clone();             // Arc<dyn CommandRunner>, Send + Sync
    for chunk in hosts.chunks(16) {
        // ... per-host diagnostics `begin` ...
        let batch: Vec<ExecResult> = thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|h| {
                let runner = runner.clone();
                let cmd = cmd.clone();
                let h = h.clone();
                s.spawn(move || to_result(&h, runner.run_ssh(&h, &cmd)))
            }).collect();
            // join all
        });
        // ... per-host diagnostics `record_started` ...
    }
    Ok(results)
})?;
```

`effect_fanout` is the dry-run gate: in a dry run it records `cmd` once per host and returns a
synthetic ok per host without calling the closure; in a live run it hands the hosts to the
closure above.

The runner is its own `Arc<dyn CommandRunner: Send + Sync>`, so each thread clones the `Arc`
and blocks on its own `ssh` independently; no shared lock sits between the threads, so the
fan-out does not serialize.

The same discipline is followed everywhere: **never hold a lock across a blocking command or
disk I/O.** The typed container reads run their probe on a `ctx.runner.clone()`; `state_set`
and `state_del` lock the `StateStore` only for the `set`/`del` call itself; transaction
compensations are popped one-at-a-time under a short lock and invoked **after** the lock is
released (see the transaction section).

---

## The `CommandRunner` trait (testability)

`runner.rs` abstracts "run a command somewhere" behind a trait so builtins never spawn a
process directly and tests need no real host:

```rust
pub trait CommandRunner: Send + Sync {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput;
    fn run_local(&self, cmd: &str) -> RawOutput;
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput;
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput;
}
```

`RawOutput { stdout, stderr, exit_code }` is the low-level result; builtins wrap it into the
script-visible `ExecResult` via `to_result(host, raw)`.

- **`RealRunner`** (a zero-field unit struct) is production. `run_ssh` spawns
  `ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10
  -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -- <host> <cmd>` with `host` passed through
  **verbatim** — no hand-resolution against `~/.ssh/config` (the old
  `SshConfig::resolve_host` step only understood `HostName`/`User` and silently dropped `Port`,
  `IdentityFile`, `ProxyJump`, etc.; the real `ssh` binary now does its own full config
  resolution, same as a plain `ssh <alias>`); `run_local` spawns `sh -c <cmd>`. The
  `*_stdin` variants pipe a payload to the child's stdin and close it, so secrets and file
  bodies are delivered **off-argv** (never visible in `ps`).
- **`FakeRunner`** (`#[cfg(test)]`) records every call as a string (`"ssh web1: uptime"`,
  `"ssh-stdin web1: <cmd> <<< <stdin>"`, …) and replays a canned `RawOutput`. Tests assert
  on `fake.calls()` to prove which commands a script would run, and that stdin payloads
  stayed off argv. The `Send + Sync` bound on the trait is what lets the same runner be
  shared across `ssh_exec_all`'s fan-out threads.

Because `Send + Sync` is mandated, a custom runner used in a test (e.g. `TrueRunner` in
`probe.rs`'s tests, which answers `"true"`/`"healthy"` to inspect probes) can drive the
parallel and dry-run paths identically.

---

## Builtins: registered closures capturing `SharedCtx`

`builtins/mod.rs::register_builtins` calls each module's `register(engine, ctx.clone())`.
A builtin is a Rhai-registered closure that **captures a clone of the `SharedCtx`**:

```rust
let ctx = ctx.clone();
engine.register_fn(
    "ssh_exec",
    move |host: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
        effect(&ctx, "ssh", Some(host), &format!("ssh_exec {host}"), cmd,
               |c| to_result(host, c.runner.run_ssh(host, cmd)))
    },
);
```

`effect` prechecks the command (secret guard and trace), then `dispatch` either records it
and returns a synthetic ok (`EffectMode::DryRun`) or runs the closure around a diagnostics
journal entry (Live). The closure reads `c.runner` directly; the blocking `run_ssh` call holds
no lock.

Each `register(...)` body re-clones `ctx` per closure (`let ctx = ctx.clone();` inside its
own `{ }` block) because the closure takes ownership of its capture. The result is that
**every builtin shares one `RunCtx`** — mode, runner, state, plan, txn — without any
global statics.

The complete builtin surface (exact names and signatures):

**Exec** (`builtins/exec.rs`):

- `ssh_exec(host, cmd) -> ExecResult` — mutating remote command.
- `ssh_probe(host, cmd) -> ExecResult` — read-only remote command; **still executes in
  dry-run** (it's a read).
- `local_exec(cmd) -> ExecResult` — mutating local command.
- `ssh_exec_all(hosts, cmd) -> [ExecResult]` — parallel fan-out; never aborts on a single
  host's failure; rejects a non-string host element loudly (won't run `ssh ""`).
- `ssh_exec_stdin(host, cmd, stdin) -> ExecResult` — off-argv stdin delivery.
- `local_exec_stdin(cmd, stdin) -> ExecResult`.
- `write_remote(host, content, remote_path) -> ExecResult` — writes a `0600` file via
  `umask 077; cat > '<path>'` with `content` on stdin (never on argv).

**HTTP** (`builtins/http.rs`):

- `http_get(url) -> HttpResponse`
- `http_post(url, body) -> HttpResponse` (sends `Content-Type: application/json`)
- `http_get` also takes `(url, headers)` and `(url, headers, timeout_secs)`; the default
  timeout is 30s. In **dry-run** `http_get` is a read: it runs for real and records a `check`
  (`GET <url> -> <status> (probed live)`). `http_post` and the other write verbs
  short-circuit to a synthetic `200`, record a `check`, and never touch the network.

**Probes** (`builtins/probe.rs`) — typed reads of the host's real state, the same in both
modes (see the dry-run section):

- `container_running(host, name) -> bool`
- `container_healthy(host, name) -> bool`
- `container_image_id(host, tag) -> String` (`""` when absent)
- `port_open(host, port) -> bool`
- `pick_port(host, base) -> i64`

`is_dry_run() -> bool` is registered from `builtins/exec.rs`.

**State** (`builtins/state.rs`):

- `state_get(key) -> String | ()` — `()` when absent.
- `has_state(key) -> bool`
- `state_set(key, value)` — atomic persist (records in dry-run).
- `state_del(key)` — atomic persist (records in dry-run).
- `state_all() -> Map`

**Util** (`builtins/util.rs`):

- `join(array, sep) -> String` — Rhai has no `Array::join`; elements stringified.
- `sleep(seconds)` — **skipped entirely in dry-run**; otherwise sleeps if `seconds > 0`.
- `nrg_env(name) -> String` — required env var; **throws** if unset.
- `env_or(name, default) -> String` — env var with fallback.

**Secrets** (`secret.rs`): `secret(name) -> Secret`, `reveal(secret) -> String`,
`sh_quote(x) -> String` (string or Secret). **Transactions** (`transaction.rs`):
`transaction(body)`, `on_rollback(cb)`.

---

## `EffectMode`: Live vs DryRun

`EffectMode` (in `context.rs`) is `Live` or `DryRun`. The mode is set once on the `RunCtx`
(`wire_run` flips it to `DryRun` when `--dry-run` is passed) and read by each builtin.

Behavior is classified **per builtin**, not globally:

- **Mutating** builtins (`ssh_exec`, `local_exec`, `ssh_exec_all`, the `*_stdin` ones,
  `write_remote`) in dry-run **record** a `PlannedAction` and return a synthetic-ok result
  instead of executing. The mode check lives in the helpers of `builtins/exec.rs`, not in
  each builtin:
  - `dispatch` is the single-host path AFTER the caller's secret guard and trace. In dry-run
    it records and returns synthetic ok; a live run is journaled around the real call.
  - `effect` (the command builtins) runs `precheck` (reject a leaked `Secret`, then the
    redacted trace line) before calling `dispatch`; `write_remote` does the same via
    `precheck_with`. `upload_file`/`download_file` run their own `assert_no_secret_leak`
    and then call `dispatch`.
  - `effect_fanout` (used by `ssh_exec_all`; its caller has already prechecked the command)
    does not call `dispatch`: in dry-run it records one action per host via `dry_run_each`,
    and a live run hands the hosts to its `real` closure.
  - `dry_run_record` / `dry_run_each` guard the record-only mutations (locks, HTTP write
    verbs, `on_rollback`, steps, and `state_set`/`state_update`/`state_del`, which record and
    then apply to the in-memory overlay store).
- **Read** builtins still read: `ssh_probe` always runs; `state_get`/`has_state`/`state_all`
  read the state store (an in-memory overlay in a dry run, so `state_set` is visible to a
  later `state_get`); the typed container reads, `pick_port` and `http_get` probe the real
  host or URL.
- **HTTP write verbs** short-circuit to a synthetic `200`.
- **`sleep`** is skipped in dry-run.

### Typed reads and the absent answer

Nothing models host state across steps: a read after a planned mutation returns the state from
before the run, and the stdlib guards its own read-after-write points with `is_dry_run()`.

`probe.rs` holds the typed reads. Each runs one probe on a `ctx.runner` clone with no lock
held, then goes through `read_or_assume`. In a live run that returns the probe result
untouched: a probe that failed to run (ssh failure, missing runtime CLI, signal) throws, and only
a genuine "no such container/image" is an absent answer. In a dry run a probe that fails to run
records a `check` row `[probe failed — assuming <x> absent] <error>` and returns the absent
answer (`false` or `""`), so a plan can be previewed from a machine that cannot reach the hosts.
`pick_port` is a plain scan in both modes and throws when it cannot find a free port; the
stdlib does not call it in a dry run and uses the placeholder `<auto>` instead.

The waits (`wait_healthy`, `wait_port`, `wait_container_healthy`) are stdlib functions that check
`is_dry_run()` and return success without probing.

---

## Transactions: the compensation stack and LIFO unwind

`transaction.rs` implements a best-effort, error-isolated compensation mechanism. The state
lives in `TxnState { comps: Vec<FnPtr>, depth: usize }` (a field of `RunCtx`).

```rhai
transaction(|| {
    ssh_exec(host, "docker run -d --name app-new img");
    on_rollback(|| { ssh_exec(host, "docker rm -f app-new"); });
    if !container_healthy(host, "app-new") { throw "unhealthy"; }
    // ... promote ...
});
```

- **`on_rollback(cb)`** pushes the `FnPtr` `cb` onto `txn.comps` (Live). In **dry-run** it
  records a `rollback` plan action and is **never invoked** (compensations don't run in a
  plan).
- **`transaction(body)`** takes a `NativeCallContext` as its first parameter — this is what
  lets it invoke the script-level `FnPtr` bodies via `body.call_within_context::<()>(&ctx, ())`
  inside the engine that's currently running. On entry it bumps `depth` and remembers
  `mark = comps.len()`. Then it runs `body`:
  - **On `Ok`:** decrement `depth`. If we're the **outermost** transaction (`depth == 0`),
    `truncate(mark)` to drop our comps. A nested success **keeps** its comps so an enclosing
    transaction's later failure still unwinds them (nested transactions flatten into the
    outer one).
  - **On `Err`** (a `throw` propagated out of the body): decrement `depth`, then **unwind**.

The unwind is a **pop-loop**, not a snapshot:

```rust
loop {
    let comp = {                                  // short lock per iteration
        let mut t = ctx.txn.lock().unwrap();
        if t.comps.len() > mark { t.comps.pop() } else { None }
    };                                            // lock released HERE
    match comp {
        Some(c) => if let Err(ce) = c.call_within_context::<()>(&context, ()) {
            eprintln!("[nrg] rollback step failed (continuing): {ce}");
        },
        None => break,
    }
}
Err(e) // re-raise the ORIGINAL failure
```

Why each detail matters:

- **Pop one, release the lock, then invoke.** A compensation that itself calls a
  builtin that touches `txn` (`on_rollback`, a nested `transaction`) would **deadlock** if
  `transaction` were still holding the `txn` lock across the call. There's a test that locks in exactly
  this no-deadlock property.
- **Popping (vs. a `split_off` snapshot)** means a compensation that registers *another*
  `on_rollback` during the unwind pushes onto the live stack and is picked up by the **next**
  pop — instead of being silently lost or leaked. Re-entrant compensations are drained, and
  the stack is left empty.
- **LIFO** — the most recently registered compensation runs first (last-in-first-out),
  matching the natural "undo in reverse order" intuition.
- **Error-isolated** — a compensation that throws is logged to stderr and the unwind
  **continues**; one failed undo doesn't abort the rest.
- The **original** error is re-raised after unwinding, so the script still sees the failure.

---

## `eval.rs`: `run_file` (`run_ast`) vs `run_fn` (append-call + `run_ast`)

`compile(path, ctx)` builds an engine via `build_engine`, then sets a
resolver collection (`engine/stdlib.rs`): `import "std/docker" as docker;` resolves to the
embedded, version-locked copy, while any other path falls through to
`FileModuleResolver::new_with_path(<file-dir>)`. **Imports are top-level**: write `import "std/x" as x;` at
the top of the file, not inside a function.

- **`run_file(path, ctx)`** — for `nrg exec`. Compiles and calls `engine.run_ast(&ast)`,
  which evaluates the module **top to bottom** (imports, config, top-level statements, side
  effects). This is the normal exec path.

- **`run_fn(path, fn_name, args, ctx)`** — for `nrg run <fn> [args...]`. It does **not** use
  `engine.call_fn`. Instead it:
  1. Reads the file content and **appends** a call statement: `"<content>\n<fn>(<arg-vars>);\n"`.
  2. Pushes each CLI arg into a `Scope` as an injected variable (`__nrg_arg_0`, … — no
     string-literal escaping needed).
  3. Compiles the augmented source and **guards**: if `fn_name` isn't among the AST's
     functions, it returns an error **before running anything** (so `nrg run <typo>` can't run
     the top-level script as a side effect and then report "not found"). `compile` parses but
     does not execute, so this guard is safe.
  4. Runs the whole thing with `engine.run_ast_with_scope(&mut scope, &ast)`.

  **Why append-call + `run_ast` instead of `call_fn`?** With nested module imports — e.g.
  `deploy.rhai` imports `docker.rhai`, which itself imports `runtime.rhai`, and the target
  function's body makes a qualified module call like `deploy::deploy(...)` — Rhai's `call_fn`
  **fails to resolve** the function ("Function not found"). Evaluating the appended call via
  `run_ast` runs through the same top-level resolution path as `nrg exec` and resolves the
  nested imports correctly. There is a regression test (`run_fn_resolves_calls_into_a_module_
  that_itself_imports`) pinning this.

Both paths run the top level first (so `import`s and config execute), then either fall off the
end (exec) or hit the appended call (run). A thrown error or a parse error surfaces as `Err`,
**redacted** against the secret set before it's returned (so a secret in a command's stderr
can't leak through an error message).

`list_functions(path)` (backing `nrg tasks`) uses a plain `rhai::Engine::new()` and only
**compiles** — nothing runs and no imports resolve — to list the script-defined functions.

---

## State: locking and atomic writes (`state.rs`)

`StateStore` is the persistent key/value deploy state at `<root>/.energize/state.json`,
versioned (`{ "version": 1, "data": { … } }`).

**Project root** is found by walking up from CWD looking for a marker
(`.energize`, `energize.toml`, `.nrg-key`) — `.git` is deliberately **not** a marker, so we
never plant state at an unrelated VCS root. The search never goes above `$HOME`, and `nrg`
**refuses** to use `$HOME` itself as a markerless root.

The directory a marker is **accepted** in must also be one the invoking user controls — owned
by our euid and not writable by other users (`src/trust.rs`, shared with the age-key search in
`src/secrets/mod.rs` so both walks apply one rule). The `$HOME` bound only fires when `$HOME`
really is an ancestor, so from `/tmp`, `/srv`, `/opt` or a CI workspace the walk reaches `/`,
and the root it picks supplies the script we execute, the secrets we read and the
state/audit files we write. A marker directory that fails the check is a
hard error naming it and the reason, never a silent fall-through to another candidate. Only the
accepted directory is checked — not the ancestors walked through, not the markerless CWD
fallback — and group-writable (`0775`) is still trusted. `secret.rs`'s `lookup_secret` applies
the same check to a `secrets` / `secrets.<dest>` file, at the point that file actually
defines the key being asked for. See [safety.md](safety.md#the-root-you-adopt-must-be-yours).

**Atomic writes**: every `set`/`del` first `reload_from_disk()` (so a concurrent nested-`nrg`
write isn't clobbered when we flush the whole map), then `flush()` does
**backup → write `.tmp` → fsync file → rename → fsync dir**. `rename` is atomic on POSIX, so a
crash mid-write never publishes a torn file (which is why there's no checksum — a partial
write stays in `.tmp`). A **missing** state file is an empty store (legit first run); a
**present-but-corrupt** file is **fatal** (we refuse to run rather than silently reset deploy
history).

### Locking via `fd-lock`

A live run takes an **advisory exclusive flock** on `<root>/.energize/state.lock` to serialize
concurrent mutating runs. The wiring is in `src/cli/exec.rs::wire_run`:

- `open_lock(root)` returns an `fd_lock::RwLock<File>`; calling `.write()` on it takes the
  exclusive lock, released when the guard drops. `wire_run` `Box::leak`s the lock so the guard
  can be `'static` (held for the whole process; released on exit).
- Before blocking, it `try_write()`s so it can print *"Waiting for the state lock…"* if held.
- **Re-entrancy**: the holder sets `NRG_STATE_LOCK` to `"<canonical-root>#<pid>"`
  (`lock_env_value`) — the symlink-resolved root path plus its own PID. A nested `nrg` (e.g.
  from a pre-deploy hook) reads that env var via `lock_is_reentrant`, which requires BOTH the
  root to match AND the recorded PID to still be a live process (`pid_is_alive` — `/proc/<pid>`
  existence on Linux, `kill -0` elsewhere) and, only then, **skips** acquiring the lock —
  avoiding self-deadlock. The PID-liveness check exists so a *leaked* env var (one naming the
  right root but whose original process has already exited — e.g. a CI runner that doesn't
  reset its environment between unrelated job steps) is never mistaken for a live ancestor; see
  `docs/safety.md`'s "Advisory flock + re-entrancy" for the full contract and its known limits.
  `reload_from_disk` is what keeps a genuinely nested writer from clobbering the parent's keys.
- **Dry-run takes NO lock and writes NO state.** It loads a `StateStore::load_overlay(root)`:
  an in-memory copy seeded from disk whose `flush` is a no-op (`root == None`), so
  `state_set`/`state_get` stay self-consistent through the plan without ever touching disk.

---

## Secrets: the redaction boundary (`secret.rs`)

`Secret(String)` is a tagged value that is **deliberately not** convertible to `String` in
scripts. Its hand-written `Debug` prints `Secret(***)` (a derived one would leak the
plaintext into Rust error messages and container `{:?}`). `to_string()`/`to_debug()` on a
`Secret` return `"***"`.

- **`secret(name) -> Secret`** looks up the `NRG_SECRET_<UPPER>` env var (plaintext), then
  `.energize/secrets[.<dest>]` (`KEY=ENC[...]`, decrypted with the discovered `.nrg-key`). It
  **throws** if missing, refused, or shorter than
  `MIN_SECRET_LEN` (6) — too-short secrets can't be safely substring-redacted. On success it
  registers the plaintext in `ctx.secrets` for redaction.
- **You cannot concatenate a `Secret` into a string.** Rhai would otherwise auto-stringify it
  via `to_string()` (= `"***"`) and silently produce a broken `… + ***` command. The `+`
  operator for `(str, Secret)`, `(Secret, str)`, `(Secret, Secret)` is registered to
  **throw**, forcing the explicit safe path:
  - **`sh_quote(secret)`** — POSIX single-quote-escaped, the only safe way to put a secret on
    a shell argument. (`sh_quote` works on plain strings too.)
  - **`reveal(secret)`** — explicit plaintext un-wrap, used when you genuinely need the raw
    value (e.g. stdin payloads).

### The redaction boundary

`redact(text, secrets)` replaces every registered plaintext (≥ 6 chars, longest-first for
determinism) with `***`. It's substring-based, so it **cannot** catch a secret that was
*transformed* before reaching the output (e.g. base64-encoded) — an accepted limit; the
`Secret` type is the real guard. Redaction is applied at every output sink:

- `on_print` / `on_debug` (stderr) — in `build_engine`.
- The `--trace` lines in exec builtins (`precheck_with` redacts via `ctx.redacted`).
- Thrown errors before they're returned from `run_file` / `run_fn`.
- **The dry-run plan log**, redacted centrally in `RunCtx::record(kind, host, detail)`:

  ```rust
  pub fn record(&self, kind: &str, host: Option<&str>, detail: String) {
      let detail = secret::redact(&detail, &self.secrets.lock().unwrap());
      self.plan.lock().unwrap().push(PlannedAction { kind, host, detail });
  }
  ```

  This is the important one: the plan prints to **stdout** (via `render_plan`), bypassing
  `on_print`. Redacting at the single `record` call site means **every** recorded action is
  covered, so a `reveal()`'d secret stored to state or echoed in a command can't leak into
  the printed plan. `render_plan` assumes its input is already redacted.

---

## CLI entry: `src/cli/exec.rs`

`nrg exec` and `nrg run` share `wire_run(dry_run)`:

1. `find_project_root()`.
2. Take the advisory lock (unless dry-run or re-entrant); set `NRG_STATE_LOCK`.
3. Load state — real `StateStore::load` (live) or `load_overlay` (dry-run).
4. Build the `SharedCtx` with a `RealRunner`; flip mode to `DryRun` if requested.

`execute(args)` resolves the file (default search: `Energize.rhai`, then `energize.rhai`),
calls `eval::run_file`, and on dry-run prints `render_plan` at the end. Exit code is `0` on
`Ok`, `1` on any `Err`.

### Failure contract (exit codes)

Exec builtins fold a non-zero command into `ExecResult.ok == false`; they **do not** abort
the script themselves. A script signals failure by `throw`ing — an uncaught `throw` (or a
parse error) surfaces from `run_file`/`run_fn` as `Err`, which the CLI maps to exit `1`. The
stdlib wraps every fallible call with `if !r.ok { throw … }`, so real deploys exit non-zero on
failure. A hand-written script that runs `ssh_exec(...)` and ignores `r.ok` exits `0` — by
design (it chose not to check). Automation that cares about command failure must use the
stdlib or check `.ok` and `throw` itself.

### Concurrency note

`main` is a plain `fn main()` — the crate has no async runtime (no tokio). `execute` is
**synchronous** and the engine blocks the calling thread: `ssh`/`sh`/`docker` are spawned
with `std::process::Command`, and fan-out builtins (`ssh_exec_all`, `http_patch_all`) run
their per-host work in parallel with `std::thread::scope`, joining before they return.

---

## Writing Rhai for this engine (quick reference)

These are the script-side rules the engine enforces — worth knowing when reading the stdlib:

- `import "std/x" as x;` goes at the **top level** (`std/` is the embedded stdlib; any other
  path is a file relative to the running file's directory).
- Config is plain Rhai object maps: `#{ image: "ghcr.io/app", hosts: ["web1", "web2"] }`.
  There are **no keyword arguments** — builtins take positional args.
- Failure is `throw "message";` (there is no `fail`).
- `trim()`/`make_lower()` **mutate** the string in place and return `()` (Rhai semantics) —
  *don't* use them as expressions (the result is `()`). Bind the string to a variable, call
  `trim()` on that variable, then read the variable.
- `state_get(key)` returns `()` when absent. Rhai requires **`bool`** conditions, so test
  presence with `state_get(k) != ()` or `has_state(k)` — **not** `if state_get(k) { … }`,
  which raises a runtime type error.
- A `Secret` **cannot** be `+`-concatenated. Use `sh_quote(secret)` for a shell argument or
  `reveal(secret)` for explicit plaintext.
- `join(array, sep)` exists because Rhai has no `Array::join`.

## Deployment safety additions

`builtins/transfer.rs` owns the file-transfer interface and atomic remote-write command;
`runner.rs` transports binary bytes without exposing payloads to Rhai or logs.
`builtins/preflight.rs` executes explicit syntax/read-only/temporary-write declarations.
`diagnostics.rs` performs incremental byte redaction and records bounded execution steps
in `RunCtx::steps`; `cli/exec.rs` includes these in live audit entries. Existing execution
methods remain available; named `ssh_step`/`local_step` opt into streaming and throwing
on failure. See [API contracts](builtins.md#deployment-safety-apis) and
[validation coverage and limits](deployment-validation.md).

### Execution options and incremental run history

`builtins/steps.rs` validates structured options and explicit retry declarations.
`RealRunner` accepts a shared interrupt flag and per-command deadlines, drains
stdout/stderr concurrently, flushes streamed stdout, and terminates the local
process group on interruption. `RunCtx` consumes the pending flag for compensation
and retains a sticky interrupted result so a final command cannot report success.

`journal.rs` appends and syncs start/finish metadata to `.energize/runs.jsonl` during
live execution. The existing final audit entry links to its run ID. Step starts
have no exit code; incomplete records do not authorize replay. Dry runs create
neither journal nor audit files. Remote configured steps frame a private script
and stdin file over SSH; no environment value or input body goes on argv.
