---
title: Authoring Guide
nav_order: 8
---

# Authoring Energize.rhai files

Energize (`nrg`) drives deployments from a single [Rhai](https://rhai.rs) script.
You write an `Energize.rhai` that orchestrates SSH, containers, a reverse proxy,
health checks, and persistent state by `import`ing the bundled stdlib in `lib/`
and calling its functions.

This guide is the practical companion to those modules: how to structure a file,
the config-map calling convention, and — most importantly — the Rhai gotchas that
bite everyone the first time. Every behavior below is taken from the runtime
(`src/engine/`) and the stdlib (`lib/*.rhai`), not from aspiration.

> Scope note: the only reverse proxy the stdlib ships is **kamal-proxy**
> (`lib/proxy.rhai`). There is no nginx/traefik/caddy/TLS-provisioning module —
> if you want one, you write a module like `lib/proxy.rhai` yourself. There is
> also no Starlark or bash runtime; the orchestration language is Rhai, full stop.

---

## Running a file

Two entry points, both discover `Energize.rhai` (then `energize.rhai`) in the
current directory if you don't pass a path:

```sh
nrg exec                 # evaluate the file top-to-bottom (side effects happen)
nrg exec deploy.rhai     # ...or a specific file
nrg run deploy web1 web2 # call fn deploy(...) with CLI args (as STRINGS)
nrg tasks                # list the callable functions in the file
nrg exec --dry-run       # print the plan of side effects without executing
nrg run deploy --dry-run # dry-run a function call
```

- `nrg exec` runs the whole top level. Use it for a script whose top level *is*
  the deploy (like `lib/examples/Energize.rhai`).
- `nrg run <fn> [args...]` evaluates the top level first (so `import`s, config,
  and `set_runtime(...)` run), then calls `<fn>` with the trailing CLI args.
  A missing function aborts **before** the top level runs anything, so
  `nrg run <typo>` never fires side effects.
- `nrg tasks` parse-only lists `fn` definitions; nothing executes and no
  `import`s resolve.

Set `NRG_TRACE=1` to echo every `ssh_exec`/`local_exec` command to stderr
(secrets redacted).

---

## File skeleton

```rhai
// imports MUST be at the top level — see the gotcha below.
import "lib/runtime"  as rt;
import "lib/registry" as registry;
import "lib/deploy"   as deploy;

// pick the container runtime once, up front, before any lib fn reads it.
rt::set_runtime("auto");          // or "docker" / "podman" / "nerdctl"

// configuration as plain let-bindings.
let SERVICE   = "myapp";
let IMAGE     = "ghcr.io/myorg/myapp";
let VERSION   = env_or("DEPLOY_TAG", "latest");
let HOSTS     = ["deploy@10.0.0.1", "deploy@10.0.0.2"];

// orchestrate.
registry::registry_login_all(HOSTS, "ghcr.io", "deploy", secret("REGISTRY_PASSWORD"));
deploy::deploy(HOSTS, IMAGE + ":" + VERSION, SERVICE, #{
    container_port: 3000,
    health_path:    "/up",
});
```

To make functions callable with `nrg run`, define them at the top level:

```rhai
import "lib/deploy" as deploy;

fn ship(version) {
    deploy::deploy(["web1", "web2"], "ghcr.io/org/app:" + version, "app", #{});
}

fn rollback() {
    deploy::rollback(["web1", "web2"], "app");
}
```

```sh
nrg run ship v42
nrg run rollback
```

---

## The `import` convention (and the gotcha that silently no-ops)

```rhai
import "lib/docker" as docker;
docker::docker_pull(host, tag);
```

The module resolver is anchored at the directory of the file you run, so
`import "lib/docker"` resolves to `<file-dir>/lib/docker.rhai`. Reference its
functions with the `module::fn` syntax.

### GOTCHA: `import` MUST be at file top level

An `import` written *inside a function body* compiles fine and **silently does
nothing useful** — the alias is not visible the way you expect, and you'll get a
"function not found" error at the qualified call, or worse, confusing behavior.
Put every `import` at the top level of the file.

```rhai
// GOOD — top level
import "lib/docker" as docker;
fn pull_all(hosts, tag) {
    for h in hosts { docker::docker_pull(h, tag); }   // resolves fine
}
```

```rhai
// BAD — import inside a function (do NOT do this)
fn pull_all(hosts, tag) {
    import "lib/docker" as docker;     // does NOT give you a usable module here
    for h in hosts { docker::docker_pull(h, tag); }   // fails
}
```

### Imports are per-file, NOT inherited

Each `import` yields a **fresh** module instance, and imports do **not** flow
from the caller into a module. That's why every stdlib module re-imports
everything it touches (`lib/deploy.rhai` imports `docker`, `proxy`,
`healthcheck`, `runtime` itself). You only `import` what *your* file calls
directly.

Because module instances are fresh, a module can't share a mutable global with
another module. The runtime choice is therefore stored in the process-global
state store (see `set_runtime` below), not in a module variable.

---

## Config maps instead of keyword arguments

Rhai has **no keyword arguments and no default parameters**. Every stdlib
function that would want optional args takes a single object map (`#{...}`) as
its last parameter, plus a shorter overload with the map omitted.

```rhai
// these are equivalent:
docker::docker_build("app:v1");
docker::docker_build("app:v1", #{ context: ".", dockerfile: "Dockerfile" });

deploy::deploy(HOSTS, IMAGE, SERVICE);          // defaults
deploy::deploy(HOSTS, IMAGE, SERVICE, #{ container_port: 3000, health_path: "/up" });
```

Maps are also how you express nested options like ports, env, and volumes:

```rhai
docker::docker_run(host, image, name, #{
    ports:   #{ "8080": "3000" },     // host_port: container_port (both strings)
    envs:    #{ "RAILS_ENV": "production" },
    volumes: #{ "/data": "/var/lib/app" },
    network: "appnet",
});
```

### Reading config inside a function

Functions read optional keys with the `contains`/else-default idiom. There is no
"get with default" helper — this pattern *is* the idiom:

```rhai
fn my_step(host, cfg) {
    let attempts = if cfg.contains("attempts") { cfg.attempts } else { 30 };
    let path     = if cfg.contains("health_path") { cfg.health_path } else { "/up" };
    // ...
}
```

Note the order: `if cfg.contains("k") { cfg.k } else { default }`. Don't reach
for `cfg.k ?? default` or a missing-key access — a missing map key in Rhai is its
own error class, and `contains` is what every stdlib module uses.

---

## Failure and exit-code contract

**The only way to signal failure is to `throw`.** Rhai has no `fail()` builtin
(if you remember `fail(...)` from elsewhere, it's `throw "message"` here).

- A `throw` that isn't caught surfaces from `nrg exec`/`nrg run` as an error and
  the process exits **1** (secrets in the message are redacted first).
- A `try { ... } catch (e) { ... }` swallows the throw — the process exits 0
  unless you re-`throw`.

### GOTCHA: an unchecked `r.ok` exits 0

`ssh_exec`/`local_exec` return an `ExecResult`; a failing command does **not**
throw on its own. If you ignore `.ok`, a failed command leaves the script running
and the process exits 0 — a "successful" deploy that did nothing.

```rhai
// BAD — failure is invisible, exit code is 0
ssh_exec(host, "systemctl restart app");

// GOOD — turn a non-zero command into a thrown failure
let r = ssh_exec(host, "systemctl restart app");
if !r.ok {
    throw "restart failed on " + host + ":\n" + r.stderr;
}
```

The stdlib already wraps its own fallible calls this way, so calling
`deploy::deploy(...)` and friends gives you the correct non-zero exit on failure.
The `if !r.ok { throw ... }` discipline is only on you when you call the raw
`ssh_exec`/`local_exec` builtins directly.

`ExecResult` fields: `.stdout`, `.stderr`, `.exit_code` (int), `.host`,
`.ok` (true iff `exit_code == 0`).
`HttpResponse` fields: `.status` (int), `.body`, `.ok` (true iff 2xx).

---

## String methods that MUTATE in place (a classic trap)

Rhai's `String` methods like `trim()` and `make_lower()` **mutate the string in
place and return `()` (unit)** — they are *not* expressions that return the new
string. Using them as expressions silently gives you `()`.

```rhai
// BAD — `out` becomes () (unit), not the trimmed string
let out = r.stdout.trim();

// GOOD — mutate, then use the variable
let out = r.stdout;
out.trim();           // mutates `out`
out                   // now the trimmed value
```

```rhai
// BAD
let os = r2.stdout.make_lower();

// GOOD
let os = r2.stdout;
os.make_lower();
if os.contains("orbstack") { ... }
```

This is exactly how `lib/runtime.rhai` (`auto_detect`) and `lib/deploy.rhai`
(`timestamp`) handle it. If you see `trim()`/`make_lower()` used as the value of
a `let`, it's a bug.

---

## Arrays have no `.join()` — use the `join` builtin

Rhai arrays do **not** have a `.join(sep)` method. Energize provides a global
`join(array, sep)` builtin instead. Non-string elements are stringified.

```rhai
// BAD — no such method
let cmd = parts.join(" ");

// GOOD
let cmd = join(parts, " ");
```

```rhai
let parts = [rt::container_cmd() + " run -d", "--name " + name, image];
let cmd = join(parts, " ");        // "docker run -d --name web app"
```

Array methods that *do* exist and are used throughout the stdlib: `.push(x)`,
`.len()`, `.filter(|x| ...)`, `.map(|x| ...)`, `.contains(x)`, `.is_empty()`,
indexing `arr[i]`, and `for x in arr { ... }`. Maps have `.keys()`,
`.contains("k")`, and `m[k]` / `m.k` access.

---

## Persistent state: absent is `()`, never test truthiness

`state_set(key, value)` and `state_get(key)` back Energize's persistent store
(used by `deploy` to remember the live image, previous image, per-host port, and
proxy target across runs).

`state_get(key)` returns the stored **string**, or `()` (unit) when the key is
absent.

### GOTCHA: `if state_get(x)` is a runtime error

Rhai conditions must be `bool`. A `String` (or `()`) in an `if` raises a type
error — so `if state_get(x) { ... }` crashes whether or not the key exists. Test
presence explicitly:

```rhai
// BAD — runtime type error (condition isn't a bool)
if state_get("app.image") { ... }

// GOOD — explicit presence check
if has_state("app.image") {
    let img = state_get("app.image");
    // ...
}

// also GOOD — compare against unit
let img = if state_get("app.image") != () { state_get("app.image") } else { "" };
```

Prefer `has_state(key)` for the check; use `!= ()` when you want the value and a
fallback in one expression. Other state builtins: `state_del(key)`,
`state_all()` (returns a map of everything).

### Ephemeral per-run session store: `session_set`/`session_get`/`has_session`

`state_set`/`state_get` are **durable** — they persist to
`.energize/state.json` and are still there on your NEXT invocation of this
project. That's exactly right for deploy history, but wrong for a value that
should only be shared across the several `import`s **within one script run**
and otherwise forgotten (robustness review R27 found `lib/runtime.rhai` had
been misusing `state_set` for precisely this, causing a `set_runtime("podman")`
from one run to silently keep affecting a LATER run that never called
`set_runtime()` at all).

`session_set(key, value)` / `session_get(key)` / `has_session(key)` have the
identical shape to their `state_*` counterparts (same absent-is-`()` gotcha
applies to `session_get`) but never touch disk — the value lives only in this
process's memory and is gone the moment `nrg` exits. Reach for these instead of
`state_set`/`state_get` whenever the value is a this-run-only configuration
choice rather than something that should outlive the run.

---

## Secrets: can't be printed or concatenated

`secret("NAME")` resolves a secret from (in order) the `NRG_SECRET_<UPPER>` env
var, `.energize/secrets.<dest>` (when `--dest` is active), `.energize/secrets`,
then `.env` (`KEY=VALUE` lines), and returns a tagged `Secret`. A `CMD[command]`
value runs `command` locally and uses its stdout as the value — the fetch-adapter
integration point for 1Password/Bitwarden/Vault/Doppler/etc; an `ENC[...]` value
is decrypted via the discovered `.nrg-key`. It **throws** if the secret is
missing, if a `CMD[...]` fetch fails, or if the final value is shorter than 6
characters.

A `Secret` is deliberately not a string:

- Printing it (`print(s)`, interpolation, debug) shows `***`, never the plaintext.
- **Concatenating it throws.** `"x" + secret("Y")` is a hard error, on purpose —
  it stops you from silently building `... + ***` into a command. The error tells
  you to use `sh_quote` or `reveal`.

Three ways to use a secret's value:

```rhai
let pw = secret("DB_PASSWORD");

// 1. As a shell argument — sh_quote() POSIX-quotes the plaintext safely.
let r = ssh_exec(host, "psql -c " + sh_quote(pw));

// 2. As explicit plaintext — reveal() un-wraps it (e.g. into an env map value).
deploy::deploy(HOSTS, IMAGE, SERVICE, #{
    envs: #{ "DATABASE_PASSWORD": reveal(pw) },
});

// 3. Off-argv via stdin — the safest path for registry/login passwords.
//    Pass reveal(pw) as the stdin arg; it never touches argv or the trace.
ssh_exec_stdin(host, "docker login ghcr.io -u deploy --password-stdin", reveal(pw));
```

```rhai
// BAD — throws: "refusing to concatenate a Secret into a string"
let cmd = "docker login -p " + secret("REGISTRY_PASSWORD");
```

The plaintext, once resolved, is registered for redaction: even if you
`reveal()` it into an env value, output and traces still mask it (best-effort
substring redaction — it can't catch a secret you transform, e.g. base64, before
it reaches output).

Prefer `ssh_exec_stdin` / `local_exec_stdin` (used by `lib/registry.rhai`) for
passwords: the payload is delivered on stdin, never on the command line. There's
also `write_remote(host, content, remote_path)` which writes content to a `0600`
file on the host via stdin — good for secret env-files.

---

## `nrg run` passes args as STRINGS — coerce yourself

Every CLI argument reaches your function as a Rhai `String`, even if it looks
like a number. Coerce explicitly if you need an int:

```rhai
fn scale(replicas_str) {
    let n = parse_int(replicas_str);    // "3" -> 3
    // ...
}
```

```sh
nrg run scale 3      # replicas_str is the STRING "3", not the int 3
```

If you do arithmetic or pass the value where an int is expected without
`parse_int`, you'll get a type error or string concatenation instead of addition.
A function argument that itself starts with `-` must come after a `--` separator
(`--dry-run` and `--file` are parsed as flags wherever they appear).

---

## Transactions and custom rollback

For multi-step operations that must unwind cleanly on failure, wrap them in
`transaction(|| { ... })` and register compensations with `on_rollback(|| { ... })`.

How it works:

- `transaction(body)` runs `body`. If `body` **throws**, every `on_rollback`
  closure registered *during that transaction* runs in **LIFO** order (last
  registered, first undone), then the original error is re-raised.
- If `body` returns normally, compensations are dropped — nothing is undone.
- Compensation errors are isolated: a throwing compensation is logged and unwind
  continues, so one failed undo can't strand the rest.
- Register the inverse **before** the effect, so a failure between registration
  and effect-completion still unwinds correctly.

```rhai
transaction(|| {
    // capture old state by value for the compensation
    let old_target = state_get(service + ".target." + host);

    let r = docker::docker_run(host, image, new_name, #{ ports: pmap });
    if !r.ok { throw "start failed: " + r.stderr; }

    // register the undo BEFORE the next risky step (health wait / switch)
    on_rollback(|| { docker::docker_remove(host, new_name); });

    health::wait_healthy_on_host(host, 8080, #{}); // throws on failure -> unwinds, removing new_name
                                                    // (checks ON `host` over SSH — R7-health)

    on_rollback(|| { proxy::proxy_deploy(host, service, old_target); });
    proxy::proxy_deploy(host, service, "localhost:8080");
});
```

This is exactly the pattern `lib/deploy.rhai` uses to make a whole rolling fleet
atomic: the entire fleet loop runs inside one `transaction`, so a failure on host
3 restores hosts 1 and 2 (proxy back to old target, new containers removed) before
re-raising. Keep per-host SSH **sequential** (`ssh_exec`, not `ssh_exec_all`)
inside such a transaction — fan-out swallows per-host failures and defeats the
atomic unwind.

For a higher-level "go back to the previous image" rollback (not mid-deploy
unwind), `lib/deploy.rhai` exposes `deploy::rollback(hosts, service)`, which
redeploys the snapshotted previous image.

---

## Dry-run behavior (`--dry-run`)

`--dry-run` prints a plan of side effects without performing them. It takes no
state lock and writes no real state (an in-memory overlay keeps `state_get`
consistent within the run). Know how each class behaves so your plans are honest:

- **Mutating exec builtins** (`ssh_exec`, `local_exec`, `ssh_exec_all`,
  `ssh_exec_stdin`, `local_exec_stdin`, `write_remote`) record a planned action
  and return a **synthetic ok** (`exit_code 0`, empty stdout/stderr). So
  `r.stdout.trim()` is empty in a plan, and any logic that branches on real
  command *output* won't see real data under dry-run.
- **`ssh_probe`** is read-only and **still runs** the real command, even in
  dry-run.
- **Container reads/mutations** go through the `sim_*` builtins (the stdlib never
  raw-`docker inspect`s over `ssh_exec`). Dry-run seeds each read from one real
  probe, then reflects stubbed mutations — so a stubbed new container reads as
  running+healthy and the deploy takes the same branches a real run would.
- **`http_get`/`http_post`** short-circuit to a synthetic `200` and record a
  check. So `wait_healthy` "passes" instantly in a plan without polling.
- **`sleep(seconds)`** is skipped entirely in dry-run (no waiting).
- **`state_set`/`state_del`** record the change into the overlay (visible to later
  `state_get` in the same run) but don't touch disk.
- **`set_runtime("auto")`** resolves to `docker` under dry-run, because the probe
  (`local_exec`) is mutating-class and returns synthetic ok. If you need a real
  probe, name the runtime explicitly (`set_runtime("podman")`) or run live.

Secret values are redacted in the plan exactly as in a live trace.

---

## Choosing the container runtime

`lib/runtime.rhai` centralizes the container CLI so every other module reads it.
Call `set_runtime(...)` once, at the top, before invoking anything that touches
containers:

```rhai
import "lib/runtime" as rt;
rt::set_runtime("docker");    // "docker" | "podman" | "nerdctl" | "auto"
rt::set_runtime();            // 0-arg overload == "auto"
```

`"auto"` probes the local machine (docker, then podman, then nerdctl) and detects
OrbStack. The choice is stored in the ephemeral per-run `session` store under
`nrg.runtime.*` (see above), which is why it's visible across all the
freshly-imported modules WITHIN this run, without leaking into a later one. An
unknown name throws.

**On macOS**, `docker_build`'s local branch and `docker_push`'s local overload additionally
prefer [Apple's `container` tool](https://github.com/apple/container) (macOS 26+, Apple Silicon)
over whatever `set_runtime(...)` resolved to, if it's installed and healthy — a SEPARATE,
local-only resolution (`rt::local_build_cmd()`/`rt::set_local_build_runtime(...)`), since Apple's
tool can never run on a remote deploy host. See
[the stdlib reference](stdlib.md#local-build-runtime-apples-container-tool-macos) for the full
detail.

---

## Quick reference: global builtins

These are available everywhere without an `import` (registered by the runtime):

| Builtin | Returns | Notes |
|---|---|---|
| `ssh_exec(host, cmd)` | `ExecResult` | MUTATING; check `.ok` |
| `ssh_probe(host, cmd)` | `ExecResult` | READ-ONLY; runs even in dry-run |
| `local_exec(cmd)` | `ExecResult` | MUTATING |
| `ssh_exec_all(hosts, cmd)` | `[ExecResult]` | parallel fan-out; never aborts on one host |
| `ssh_exec_stdin(host, cmd, stdin)` | `ExecResult` | stdin off-argv (passwords) |
| `local_exec_stdin(cmd, stdin)` | `ExecResult` | local mirror |
| `write_remote(host, content, path)` | `ExecResult` | writes a `0600` file via stdin |
| `http_get(url)` | `HttpResponse` | dry-run -> synthetic 200 |
| `http_post(url, body)` | `HttpResponse` | JSON content-type; dry-run -> 200 |
| `state_get(key)` | `String` or `()` | absent is `()` |
| `has_state(key)` | `bool` | presence check |
| `state_set(key, value)` / `state_del(key)` | `()` | persisted (overlay in dry-run) |
| `state_all()` | `Map` | everything |
| `session_get(key)` | `String` or `()` | absent is `()`; never touches disk |
| `has_session(key)` | `bool` | presence check |
| `session_set(key, value)` | `()` | in-memory only, forgotten when `nrg` exits |
| `secret(name)` | `Secret` | throws if missing/too short |
| `reveal(secret)` | `String` | explicit plaintext |
| `sh_quote(x)` | `String` | POSIX-quote a string or secret for a shell arg |
| `join(array, sep)` | `String` | the array "join" |
| `sleep(seconds)` | `()` | skipped in dry-run |
| `nrg_env(name)` | `String` | required env var; throws if unset |
| `env_or(name, default)` | `String` | env var with fallback |
| `is_dry_run()` | `bool` | mode check for cosmetic branches |
| `transaction(\|\| {...})` | `()` | unwinds `on_rollback` comps on throw |
| `on_rollback(\|\| {...})` | `()` | register a LIFO compensation |
| `throw "msg"` | — | the only way to signal failure |

The `lib/` modules (`docker`, `proxy`, `registry`, `healthcheck`, `deploy`,
`runtime`) are imported and called as `module::fn(...)` — read their source for
exact config keys; each function's doc comment lists its `cfg` shape.
