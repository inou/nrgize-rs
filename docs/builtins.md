---
title: Builtins Reference
nav_order: 4
---

{% raw %}

# Runtime builtin reference

This is the complete reference for the builtins the Energize (`nrg`) runtime registers
into the Rhai engine before it evaluates your `.rhai` scripts. Every function documented
here is registered in Rust under `src/engine/builtins/`, `src/engine/secret.rs`, and
`src/engine/transaction.rs`. Result/value types (`ExecResult`, `HttpResponse`, `Secret`)
come from `src/engine/types.rs` and `src/engine/secret.rs`.

Nothing else is "magic". There is **no** nginx / TLS / Caddy / provisioning module, and
**no** Starlark or bash layer — those are gone. `kamal-proxy` is the only proxy, and you
drive it through ordinary `ssh_exec` / the `sim_*` builtins, not a dedicated builtin.

## Contents

- [Two execution modes](#two-execution-modes)
- [Result types](#result-types)
  - [ExecResult](#execresult)
  - [HttpResponse](#httpresponse)
  - [Secret](#secret)
- [Command execution](#command-execution)
- [HTTP](#http)
- [Persistent state](#persistent-state)
- [Secrets](#secrets)
- [Transactions / rollback](#transactions--rollback)
- [Simulated container / port / health (`sim_*`)](#simulated-container--port--health-sim_)
- [Utilities](#utilities)
- [Rhai gotchas](#rhai-gotchas)

---

## Two execution modes

Every effectful builtin behaves differently depending on the runtime's `EffectMode`:

- **Live** — the default. Commands run for real through the runner; HTTP requests go out
  on the wire; `state_set`/`state_del` flush to disk; `sleep` actually sleeps.
- **DryRun** — nothing mutating is executed. Mutating builtins **record a planned action**
  (printed as the plan) and return a synthetic success. Reads go through the `sim` overlay
  or a state overlay so the script still takes the same branches a real run would. HTTP
  short-circuits to a synthetic healthy `200`. `sleep` is skipped.

Where a builtin's dry-run behaviour matters, it is called out per-function below. The
classification is **by builtin**, not by inspecting the command string — e.g. `ssh_exec`
is always treated as mutating, `ssh_probe` as read-only.

---

## Result types

### ExecResult

Returned by all command-execution builtins and the mutating `sim_docker_*` builtins.
Defined in `src/engine/types.rs`. All fields are read-only getters:

| Property      | Type   | Meaning                                              |
|---------------|--------|------------------------------------------------------|
| `.stdout`     | string | Captured standard output.                            |
| `.stderr`     | string | Captured standard error.                             |
| `.exit_code`  | int    | Process exit code (`0` = success).                   |
| `.host`       | string | The host the command ran on (`""` for local).        |
| `.ok`         | bool   | Convenience: `true` iff `exit_code == 0`.            |

```rhai
let r = ssh_exec("web1", "uptime");
if !r.ok {
    throw "uptime failed on " + r.host + ": " + r.stderr;
}
print(r.stdout);   // note: `trim()` mutates in place and returns () — see the authoring guide
```

In dry-run, a synthetic `ExecResult` is returned with empty `stdout`/`stderr` and
`exit_code == 0` (so `.ok` is `true`), and `.host` set to the target host.

### HttpResponse

Returned by `http_get` / `http_post`. Defined in `src/engine/types.rs`. Read-only getters:

| Property   | Type   | Meaning                                                   |
|------------|--------|-----------------------------------------------------------|
| `.status`  | int    | HTTP status code. `0` means the request itself failed.    |
| `.body`    | string | Response body (or `"request failed: …"` when `status==0`).|
| `.ok`      | bool   | `true` iff `status` is in `200..300`.                     |

```rhai
let resp = http_get("http://localhost:13000/up");
if resp.ok { print("healthy"); }
```

Note `.ok` is **2xx only**. A `3xx`/`4xx`/`5xx` status returns `ok == false`. A non-status
transport error (connection refused, DNS, timeout) yields `status == 0` and the body
`"request failed: …"`.

### Secret

A tagged plaintext value, defined in `src/engine/secret.rs`. The whole point of the type is
that the plaintext can **never** leak by accident:

- `debug(secret)` and Rust-side `Debug` render `"***"` / `Secret(***)`, never the value.
- `to_string(secret)` and string **interpolation** (`` `${secret}` ``) produce an internal
  sentinel, not the plaintext or a plausible value — and any command containing that sentinel
  is **rejected** at the execution boundary. (Rhai swallows a `to_string` error during
  interpolation, so we can't throw there directly; the sentinel + boundary check closes the gap.)
- Concatenating a `Secret` into a string with `+` is a hard error (see below).
- The only ways to get the plaintext out are `reveal(secret)` and `sh_quote(secret)`.

You obtain a `Secret` from [`secret(name)`](#secretname---secret) and consume it with
[`reveal`](#reveal-secret) / [`sh_quote`](#sh_quote-x). See [Secrets](#secrets).

---

## Command execution

Registered in `src/engine/builtins/exec.rs`. When tracing is on, each call is logged to
stderr with any registered secret values redacted to `***`.

### `ssh_exec(host, cmd) -> ExecResult`

Run `cmd` on `host` over SSH. **Mutating.**

- **Live:** executes via the runner, returns the real `ExecResult`.
- **DryRun:** records a `ssh` planned action with the command text, returns synthetic ok
  (`exit_code 0`, empty output, `.host == host`). Does **not** execute.

```rhai
let r = ssh_exec("web1", "docker pull myapp:v2");
if !r.ok { throw "pull failed: " + r.stderr; }
```

### `ssh_probe(host, cmd) -> ExecResult`

Run a **read-only** command on `host`. Classified as a probe, so unlike `ssh_exec` it
**still executes in dry-run** — use it for inspections whose result your script branches on.
(For container/port/health reads, prefer the [`sim_*`](#simulated-container--port--health-sim_)
builtins, which keep dry-run consistent with stubbed mutations.)

```rhai
let cur = ssh_probe("web1", "docker inspect -f '{{.Id}}' app").stdout;
cur.trim();   // trims in place; `cur` is now the image id (trim() returns () — don't chain it)
```

### `local_exec(cmd) -> ExecResult`

Run `cmd` on the local machine. **Mutating.** `.host` is `""`.

- **DryRun:** records a `local` planned action, returns synthetic ok. Does not execute.

### `ssh_exec_all(hosts, cmd) -> [ExecResult]`

Run the same `cmd` on every host in the `hosts` array, **in parallel**. Returns an array of
`ExecResult`, one per host, in input order. **Mutating.**

- A single host's failure never aborts the others — you get every result and decide.
- A thread panic for one host yields an `ExecResult` with `exit_code == -1`, `stderr ==
  "thread panicked"`, and `host == ""`.
- **Non-string host elements are rejected loudly** (the call throws) rather than coerced to
  `""` — a typo'd array element can't silently become `ssh ""`.
- **DryRun:** records one `ssh-all` planned action per host, returns synthetic-ok results.

```rhai
let results = ssh_exec_all(["web1", "web2", "web3"], "docker pull myapp:v2");
let failed = [];
for r in results {
    if !r.ok { failed.push(r.host); }
}
if failed.len() > 0 { throw "pull failed on: " + join(failed, ", "); }
```

### `ssh_exec_stdin(host, cmd, stdin) -> ExecResult`

Run `cmd` on `host`, delivering `stdin` over the stdin channel — **never on argv** and never
traced (only its byte length is logged). Use this to hand a password to e.g.
`docker login --password-stdin`. **Mutating.**

- **DryRun:** records a `ssh-stdin` planned action (command only, no payload), synthetic ok.

```rhai
let pw = secret("REGISTRY_PASSWORD");
ssh_exec_stdin("web1", "docker login -u robot --password-stdin registry.example.com",
               reveal(pw));
```

### `local_exec_stdin(cmd, stdin) -> ExecResult`

Local mirror of `ssh_exec_stdin`. **Mutating.** `.host` is `""`. **DryRun:** records a
`local-stdin` action.

### `write_remote(host, content, remote_path) -> ExecResult`

Write `content` to `remote_path` on `host` as a `0600` file, delivering the content over
stdin (never on argv). Internally runs `umask 077; cat > '<remote_path>'` with the path
POSIX-quoted. Ideal for secret env-files and configs. **Mutating.**

- The byte length is logged when tracing; the content is not.
- **DryRun:** records a `write` planned action of the form `write N bytes -> <remote_path>`,
  synthetic ok. Does not write.

```rhai
let token = secret("APP_TOKEN");
let envfile = "APP_TOKEN=" + reveal(token) + "\nRAILS_ENV=production\n";
write_remote("web1", envfile, "/run/myapp/app.env");
```

---

## HTTP

Registered in `src/engine/builtins/http.rs`. Uses `ureq` with a **30-second global timeout**.

### `http_get(url) -> HttpResponse`

GET `url`.

- **Live:** performs the request. A non-2xx HTTP status is returned as that status with an
  empty body. A transport failure returns `status == 0` and a `"request failed: …"` body.
- **DryRun:** short-circuits — records a `check` action (`[assumed healthy] GET <url>`) and
  returns a synthetic `200` with an empty body, so a health-wait loop against a
  not-yet-started container doesn't fail or hang the plan.

```rhai
// poll until healthy
let healthy = false;
for _ in 0..30 {
    if http_get("http://localhost:13000/up").ok { healthy = true; break; }
    sleep(2);
}
if !healthy { throw "service never became healthy"; }
```

### `http_post(url, body) -> HttpResponse`

POST `body` to `url` with `Content-Type: application/json`.

- **Live:** same status/transport-error semantics as `http_get`.
- **DryRun:** short-circuits — records a `check` action (`[assumed ok] POST <url>`) and
  returns a synthetic `200`.

---

## Persistent state

Registered in `src/engine/builtins/state.rs`, backed by the run's `StateStore`. A key-value
string store that survives between runs. In dry-run the store is an overlay (no flush), so
reads stay consistent with writes recorded during the run.

### `state_get(key) -> string | ()`

Returns the stored string, or **`()` (unit)** when the key is absent.

> Rhai `if`/`while` conditions must be `bool`. `if state_get(k) { … }` is a **runtime type
> error**. Test presence with `state_get(k) != ()` or use [`has_state`](#has_statekey---bool).

```rhai
let last = state_get("app.version");   // () if never set
if last != () { print("last deployed " + last); }
```

### `has_state(key) -> bool`

Ergonomic presence check (returns a real `bool`, safe in conditions).

```rhai
if !has_state("app.version") { print("first deploy"); }
```

### `state_set(key, value)`

Persist `value` under `key`.

- **Live:** writes atomically to disk.
- **DryRun:** records a `state` action (`key = value`, with any registered secret redacted)
  and writes to the overlay only — no disk flush. Later `state_get(key)` in the same dry-run
  reflects the overlay value.

### `state_del(key)`

Delete `key` (deleting a missing key is fine). **DryRun:** records `del <key>`; mutates the
overlay only.

### `state_all() -> map`

Return all key-value pairs as a Rhai map (`#{ key: value, … }`).

```rhai
state_set("app.version", "v2");
let all = state_all();
print(all["app.version"]);   // "v2"
```

---

## Ephemeral session state

Registered alongside the persistent-state builtins in `src/engine/builtins/state.rs`, backed
by `RunCtx::session` — a plain in-memory map with **no disk I/O at all**, in either Live or
DryRun mode. A value set here is visible to every module `import`ed within THIS run (exactly
what `state_set`/`state_get` were being repurposed for by `lib/runtime.rhai` before robustness
review R27), but is gone the instant the process exits — it never persists across separate
`nrg exec`/`nrg run` invocations the way `state_set` does.

### `session_get(key) -> string | ()`

Returns the stored string, or **`()` (unit)** when the key is absent — same absent-is-`()`
gotcha as `state_get`; test with `!= ()` or `has_session`.

### `has_session(key) -> bool`

Ergonomic presence check.

### `session_set(key, value)`

Store `value` under `key` in memory only. Always a no-op on disk, in both Live and DryRun —
there is nothing to record into the dry-run plan either, since nothing is ever persisted.

```rhai
session_set("nrg.runtime.cmd", "podman");
if has_session("nrg.runtime.cmd") { print(session_get("nrg.runtime.cmd")); }   // "podman"
// A later, separate `nrg exec` invocation never sees this — session_set never touches disk.
```

---

## Secrets

Registered in `src/engine/secret.rs`. See the [`Secret` type](#secret) above.

### `secret(name) -> Secret`

Look up secret `name` and return a `Secret`. Resolution order:

1. Environment variable `NRG_SECRET_<NAME>` (name upper-cased).
2. `.energize/secrets.<dest>` — only if `--dest <dest>` is active (see
   [Environments / destinations](cli.md#environments--destinations)).
3. `.energize/secrets` file (`KEY=VALUE`, optional surrounding quotes, `#` comments).
4. `.env` file (same format).

Once a raw value is found (from any of the four sources above), two special framings are
applied, in order, before the usual checks:

- **`CMD[command]`** — a fetch-adapter command (roadmap 2.4): run `command` locally and use its
  (trailing-newline-trimmed) stdout as the value. This is the integration point for 1Password,
  Bitwarden, Vault, Doppler, or anything else with a CLI — e.g.
  `API_TOKEN=CMD[op read op://vault/item/field]` in `.energize/secrets`. Throws if the command
  fails, including its (trimmed) stderr in the error. **Runs even under `--dry-run`** (same as
  `ENC[...]` decryption below) — a script needs the real value to render a realistic plan, but
  this means a dry run can invoke your secret-manager CLI and requires you already be
  authenticated to it.
- **`ENC[...]`** — an encrypted token (`nrg secrets encrypt`/`seal` produce these): decrypted
  transparently via the discovered `.nrg-key`.

Throws if the secret is **not found**, or if its (post-fetch/decrypt) value is **shorter than 6
characters** (`MIN_SECRET_LEN`) — short values can't be reliably redacted from output and are
weak anyway. Registering the secret also adds its value to the redaction set so it gets masked
in traces, regardless of which of the four sources or two framings produced it.

```rhai
let pw = secret("REGISTRY_PASSWORD");   // throws if unset / < 6 chars
```

### `reveal(secret) -> string`

Explicitly un-wrap a `Secret` to its plaintext string. The only fully-plaintext escape hatch
(use sparingly — e.g. for a stdin payload).

```rhai
ssh_exec_stdin("web1", "docker login --password-stdin", reveal(pw));
```

### `sh_quote(x) -> string`

POSIX single-quote-escape a value for safe shell interpolation. Overloaded for **both** plain
strings **and** `Secret`. For a `Secret`, this is the safe way to put it on a command line —
the plaintext is wrapped in `'…'` with embedded quotes escaped. Spaces, `$`, backticks,
newlines all stay literal.

```rhai
let user_input = "a b$c`d";
ssh_exec("web1", "echo " + sh_quote(user_input));     // echo 'a b$c`d'

// secret on a command line — quoted, never concatenated raw:
ssh_exec("web1", "myctl --token=" + sh_quote(secret("API_TOKEN")));
```

### `to_string(secret)` / interpolation — rejected at the command boundary

Stringifying a `Secret` (explicitly via `to_string`, or implicitly via `` `${secret}` ``
interpolation) yields an internal sentinel, never the plaintext. Any command that contains the
sentinel is **rejected before it runs** — so a `` `docker login -p ${secret("PW")}` `` can't
silently execute with a wrong value. Use `sh_quote(secret)` for a shell argument or
`reveal(secret)` for explicit plaintext. (`debug(secret)` still renders `"***"`.)

### Why `+` with a `Secret` throws

Concatenating a `Secret` into a string is a **hard error**, by design:

```rhai
let pw = secret("PW");
"docker login -p " + pw;   // ERROR: refusing to concatenate a Secret into a string
```

Without this, Rhai would auto-stringify the secret to `"***"` and silently build a broken
command. The error forces you to choose: `sh_quote(pw)` for a shell argument, or
`reveal(pw)` for explicit plaintext.

---

## Transactions / rollback

Registered in `src/engine/transaction.rs`. A small compensation-stack: register undo
closures as you make changes, and if the transaction body throws, they run in reverse order.

### `transaction(body)`

Run the `body` closure. If it throws, **unwind** all `on_rollback` compensations registered
during the body, **LIFO**, then re-raise the original error. On success, the body's
compensations are discarded (committed).

- Unwinding is **best-effort and error-isolated**: if a compensation itself throws, the
  error is logged (`[nrg] rollback step failed (continuing): …`) and unwinding continues.
- **Nested** transactions flatten: a nested transaction that *succeeds* keeps its
  compensations so that an *enclosing* transaction's failure still unwinds them. Only the
  outermost commit discards them.
- A compensation may itself call `on_rollback` during unwind; the new entry is picked up and
  also run (the stack is drained, not snapshotted).

### `on_rollback(cb)`

Register compensation closure `cb` with the current transaction.

- **Live:** pushes `cb` onto the compensation stack. It runs only if an enclosing
  `transaction(...)` body throws.
- **DryRun:** records a `rollback` planned action (`register compensation`) and **never
  invokes** the closure — dry-run never simulates a failure path.

```rhai
transaction(|| {
    let old = ssh_probe("web1", "docker inspect -f '{{.Id}}' app").stdout;
    old.trim();   // trims in place (trim() returns ())

    sim_docker_run("web1", "myapp:v2", "app-new", "docker run -d --name app-new myapp:v2");
    on_rollback(|| { sim_docker_remove("web1", "app-new", "docker rm -f app-new"); });

    if !sim_container_healthy("web1", "app-new") {
        throw "new container unhealthy";   // <- triggers the rollback above
    }

    sim_proxy_switch("web1", "app", "localhost:13000",
                     "kamal-proxy deploy app --target localhost:13000");
    on_rollback(|| {
        sim_proxy_switch("web1", "app", old,
                         "kamal-proxy deploy app --target " + old);
    });
});
```

> `on_rollback` outside any `transaction(...)` simply pushes onto the stack and never fires.
> Compensations only run when a `transaction` body that registered them throws.

### `in_transaction() -> bool`

`true` whenever at least one `transaction(...)` is currently active (including
nested ones), `false` otherwise. Intended for a stdlib function that wraps its
own `transaction()` and does further, non-compensated work right after —
such a function is only safe to call at the top level (see "Nesting" above:
a nested transaction's compensations survive its own commit for an enclosing
transaction to unwind later, so treating that commit as final isn't safe when
nested). `deploy()` and `rollback()` (`lib/deploy.rhai`) both check this first
and refuse to run when already nested — `rollback()` has its own check
(rather than relying solely on the one inside the `deploy()` it calls
internally) because it persists state before ever reaching `deploy()`.

---

## Simulated container / port / health (`sim_*`)

Registered in `src/engine/builtins/sim.rs`. **All** container existence/state/health reads
and container/proxy mutations in the stdlib go through these typed builtins — never through
a raw `docker inspect` / `nc -z` over `ssh_exec`, which would bypass the simulation and
diverge under dry-run.

How they behave by mode:

- **Live:** each runs the real command via the runner (mutations) or a real probe (reads).
- **DryRun:** a **read** seeds lazily from exactly **one** real probe per `(host, name)` and
  thereafter reflects stubbed mutations; a **mutation** records a `PlannedAction`, applies
  the matching change to the in-memory `SimState` overlay, and returns synthetic ok. So a
  stubbed `sim_docker_run` of a NEW container makes `sim_container_running(new)` and
  `sim_container_healthy(new)` immediately true — dry-run takes the same branches a real run
  would.

The `sim_docker_*` mutators take the **exact shell command to run in live mode** as
their last argument, plus the structured arguments the simulation needs to update its model.

### `is_dry_run() -> bool`

`true` when running in dry-run. Cheap mode check for cosmetic branches (e.g. printing
`<auto>` instead of a real picked port). Registered here but applies globally.

```rhai
let port = sim_pick_port("web1", 3000);
if is_dry_run() { print("would use port <auto>"); } else { print("using port " + port); }
```

### `sim_container_running(host, name) -> bool`

Is container `name` running on `host`?

- **Live:** real `docker inspect -f '{{.State.Running}}' <name>` (true iff stdout is `true`).
- **DryRun:** seeds from one real inspect on first access, then reflects stubbed
  run/stop/rename/remove mutations.

### `sim_image_id(host, tag) -> string`

The image id for `tag` on `host`.

- **Live:** real `docker image inspect -f '{{.Id}}' <tag>` (empty string if absent).
- **DryRun:** seeds once from a real read; if unknown, returns a **branch-stable synthetic
  token** `"<tag>"` (e.g. `"<myapp:v2>"`) so id comparisons stay deterministic.

### `sim_pick_port(host, base) -> int`

Pick a free host port for a new container.

- **Live:** real `nc -z localhost <port>` scan starting at `base + 10000`, upward, returning
  the first port **not** answering (scans up to 100 candidates; falls back to `base+10000`).
- **DryRun:** deterministic symbolic port — `base + 10000`, then `+1` per subsequent pick.
  Records a `check` action; no probe. (E.g. two picks at base `3000` yield `13000` then
  `13001`.)

### `sim_docker_run(host, tag, name, cmd) -> ExecResult`

Start container `name` from image `tag`. `cmd` is the literal command run in live mode.

- **Live:** runs `cmd` via the runner (equivalent to `ssh_exec(host, cmd)`).
- **DryRun:** records an `ssh` action with `cmd`, marks `(host, name)` **running and
  healthy** in the sim, returns synthetic ok.

### `sim_docker_stop(host, name, cmd) -> ExecResult`

Stop container `name`. **DryRun:** records `ssh` + marks `(host, name)` stopped.

### `sim_docker_rename(host, old, new, cmd) -> ExecResult`

Rename container `old` to `new` (e.g. promoting `app-new` to the canonical `app`).
**DryRun:** records `ssh` + renames in the sim, so `sim_container_running(host, new)` is
true and `(host, old)` is gone.

### `sim_docker_remove(host, name, cmd) -> ExecResult`

Remove container `name`. **DryRun:** records `ssh` + clears `(host, name)` from the sim.

### `sim_docker_restart(host, name, cmd) -> ExecResult`

Restart an existing container in place (`docker restart` — no image argument, since a
restart can't change what image a container runs; see `accessory_restart` in
[`docs/deploy.md`](deploy.md)). **DryRun:** records `ssh` + marks `(host, name)` running and
healthy again, preserving its already-recorded image.

### `sim_proxy_switch(host, service, target, cmd) -> ExecResult`

Point kamal-proxy `service` at `target` (e.g. `"localhost:13000"`). `cmd` is the real
`kamal-proxy deploy …` invocation.

### `sim_http_healthy(url, timeout_secs?) -> HttpResponse`

The new-container health probe `wait_healthy` uses (as opposed to `http_get`, which is a
general-purpose builtin). `timeout_secs` defaults to 30s if omitted (robustness review R12 —
callers with their own retry loop should pass an explicit, smaller timeout so a hanging
endpoint can't blow up the loop's total budget).

- **Live:** a real `GET url` with the given timeout.
- **DryRun:** the new container isn't actually running yet, so a real probe of its (symbolic)
  port would always fail — short-circuits to a synthetic healthy `200` and records a
  `[assumed healthy] GET <url>` check.

- **Live:** runs `cmd`.
- **DryRun:** records `ssh` + stores the proxy `target` in the sim (used for read-back and
  rollback snapshots).

### `sim_wait_port(host, port) -> bool`

Despite the name, this does NOT wait/retry itself (robustness review R11 — it used to, but
that duplicated `lib/healthcheck.rhai`'s `wait_port`, its only caller, which already retries
with its own `cfg.attempts`/`cfg.interval`; the two loops compounded to up to 30x the
configured bound). Checks `port` exactly once.

- **Live:** ONE real `nc -z localhost <port>` probe; `true` if it connects, else `false`.
- **DryRun:** records a `check` action; returns `true` iff the sim marks that port occupied
  (agrees with a just-stubbed container). No probe, no sleep.

Call this in a loop yourself (see `wait_port` in `lib/healthcheck.rhai`) if you need retries.

### `sim_container_healthy(host, name) -> bool`

Same one-shot shape as `sim_wait_port` above, for the same reason (its only caller,
`wait_container_healthy`, already retries).

- **Live:** ONE real `docker inspect -f '{{.State.Health.Status}}' <name>` probe; `true` iff
  the status is `healthy`, else `false`.
- **DryRun:** records a `check` action; returns `true` iff the sim has `(host, name)` running
  **and** healthy (set by `sim_docker_run`). No probe, no sleep.

```rhai
import "lib/healthcheck" as health;

let port = sim_pick_port("web1", 3000);
let name = "app-" + port;
sim_docker_run("web1", "myapp:v2", name,
               "docker run -d --name " + name + " -p 127.0.0.1:" + port + ":3000 myapp:v2");
// health::wait_port/wait_container_healthy retry with their own cfg.attempts/cfg.interval —
// sim_wait_port/sim_container_healthy above are ONE-SHOT probes and won't wait for the
// container to actually finish starting if called directly like the old example here did.
health::wait_port("web1", port, #{});
health::wait_container_healthy("web1", name, #{});
```

> Ports are `i64` in Rhai and clamped into `0..=65535` (`u16`) before use. Don't rely on
> out-of-range values.

---

## Utilities

Registered in `src/engine/builtins/util.rs`.

### `join(array, sep) -> string`

Join an array into a string with `sep` between elements (Rhai has no built-in `Array::join`).
String elements pass through; non-strings (numbers, bools) are stringified.

```rhai
let ports = ["-p", "80:80", "-p", "443:443"];
let args = join(ports, " ");        // "-p 80:80 -p 443:443"
join([1, 2, 3], "-");               // "1-2-3"
join([], ", ");                     // ""
```

### `sleep(seconds)`

Sleep for `seconds` (int). Non-positive values are a no-op.

- **DryRun:** skipped entirely (returns immediately) so dry-runs don't wait.

### `nrg_env(name) -> string`

Read a **required** environment variable. **Throws** (aborting the script) if `name` is
unset.

```rhai
let registry = nrg_env("REGISTRY_HOST");   // aborts the deploy if missing
```

### `env_or(name, default) -> string`

Read an environment variable with a fallback. Returns `default` if `name` is unset.

```rhai
let tag = env_or("APP_TAG", "latest");
```

---

## Rhai gotchas

A few language-level things that trip people up writing deploy scripts:

- **Imports go at the top level.** Put `import "lib/docker" as docker;` at module scope, not
  inside a function or block.
- **Config is a Rhai map literal**, e.g. `#{ host: "web1", image: "myapp:v2", port: 3000 }`.
  There are **no keyword arguments** — pass a config map and read fields off it.
- **`fail` doesn't exist — use `throw`.** Errors abort the script and (inside a
  `transaction`) trigger rollback. Catch with `try { … } catch(e) { … }`.
- **`trim()` mutates in place** on a string variable. To keep the original, work on a copy,
  or read `.stdout` (which returns a fresh string each access) before trimming.
- **`state_get` returns `()` when absent**, and `()` is **not** a bool — test with
  `!= ()` or `has_state(...)`, never `if state_get(k) { … }`.
- **A `Secret` can't be concatenated.** Use `sh_quote(secret)` for a shell argument or
  `reveal(secret)` for explicit plaintext. Plain string interpolation of a `Secret` yields
  `"***"`.
- **`ssh_exec_all` rejects non-string host entries** by throwing — a wrong-typed array
  element won't silently run `ssh ""`.

{% endraw %}
