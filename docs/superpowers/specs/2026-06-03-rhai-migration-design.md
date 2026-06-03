# Design: Rhai migration + production-safety hardening for `nrg`

**Status:** approved-in-principle (pending spec review)
**Date:** 2026-06-03
**Author:** Maciek + Claude (brainstorming + verification workflow)

---

## 1. Goal

Replace **both** config languages — Starlark (`starlark_parser.rs`, 1218 LOC) and the
bash-annotation dialect (`bash_parser.rs`, 824 LOC) — with the **Rhai** embedded
scripting language, under a **single unified model**. Rewrite the entire `lib/*.star`
stdlib to `.rhai`. Implement four production-safety features that the side-effecting
evaluation model currently lacks: **dry-run**, **transactions/rollback**,
**state-locking**, and **secret-leak fixes**.

This is a clean break: no Starlark/bash back-compat (the tool is `0.1.x`).

## 2. Decisions (locked during brainstorming)

| # | Decision | Choice |
|---|---|---|
| D1 | Two-mode split | **Unify into one Rhai model.** `nrg run <fn> [args]` calls a Rhai function; `nrg exec [file]` runs the module top-level. One engine, one loader, one `Energize.rhai`. |
| D2 | Dry-run | **Effect interception** + a **simulated-state overlay** (see §6 — deepened by adversarial review). |
| D3 | Transactions | **Compensation stack** builtin (`transaction`/`on_rollback`), best-effort error-isolated LIFO unwind (see §7). |
| D4 | State locking | **Full fix**: project-root anchoring, advisory `flock`, atomic temp+fsync+rename, hard-fail on corruption (see §8). |
| D5 | Secrets | **Full bundle** + **stop passing secrets as remote env vars** (see §9 — deepened by adversarial review). |
| D6 | TUI | Keep per-host prefixed streaming; drop full-screen TUI. **Note:** verification found there is *no ratatui code* — only `crossterm` for color. So this is just a dependency cleanup. |

## 3. Architecture

### 3.1 Module map (verified inventory)

**Delete** (Starlark/bash-specific, ~3.0k LOC):
`parsing/starlark_parser.rs`, `parsing/bash_parser.rs`, `runtime/loader.rs`
(Starlark `FileLoader`), and the Starlark globals plumbing inside
`runtime/{exec,http,state,transfer,util,types,mod}.rs` (rewritten, not literally
deleted — see below).

**Rewrite to Rhai:**
- `runtime/mod.rs` → `register_runtime(engine: &mut Engine, ctx: Arc<Mutex<RunCtx>>)` replacing `register_all(&mut GlobalsBuilder)`.
- `runtime/{exec,http,state,transfer,util}.rs` → register native Rhai fns (closures capturing `ctx`) instead of `#[starlark_module]` globals.
- `runtime/types.rs` → `ExecResult`/`HttpResponse` become plain `#[derive(Clone)]` structs registered via `register_type_with_name` + `register_get` (no `StarlarkValue`/`Allocative`).
- `cli/exec.rs` → build one Rhai `Engine`, `compile_file` → `run_ast`.
- `cli/run.rs` → same engine, `call_fn(scope, ast, fn_name, args)`.
- `parsing/mod.rs` → drop the `Parser` trait + dual dispatch; `.rhai`/`.star`/`.sh` all load as Rhai (extension kept only for file discovery).

**Keep unchanged:** `ssh/config.rs`, `secrets/mod.rs` (age), `parsing/env_parser.rs`
(dotenv), `cli/{init,doctor,ssh,secrets,tasks}.rs` (doc-string updates only),
`execution/*` (the async streaming SSH runner — see §3.4).

**New:** `runtime/context.rs` (the `RunCtx`), `runtime/effects.rs` (effect classification + plan log + simulated overlay), `runtime/txn.rs` (compensation stack), `runtime/secret.rs` (the `Secret` type + redaction), `runtime/engine.rs` (engine assembly + module resolver).

### 3.2 Cargo deltas
- **Remove:** `starlark`, `starlark_derive`, `allocative`, `dupe`, `ratatui`, `crossterm`.
- **Add:** `rhai = { version = "1.25", features = ["sync"] }`, `fd-lock`.
- **`sync` is mandatory** (verified): the tool is `tokio`-async and fans SSH across
  threads, so `Engine`/`Dynamic`/`FnPtr` must be `Send + Sync`. This forces
  `Arc<Mutex<_>>` (not `Rc<RefCell<_>>`) for shared state and `Fn + Send + Sync + 'static`
  closures. The same source compiles under both feature sets — verified.

### 3.3 The per-run context (`RunCtx`)

Every side-effecting builtin is a `move` closure capturing `Arc<Mutex<RunCtx>>`.
Rhai `register_fn` closures are `Fn` (not `FnMut`), so **all mutation goes through the
Mutex** — verified requirement, not a style choice. The engine **tag**
(`set_default_tag`) is per-engine and read-only from native fns, so it is **not** a
substitute; it may only carry a small immutable run-id.

```rust
pub struct RunCtx {
    pub mode: EffectMode,            // Live | DryRun
    pub plan: Vec<PlannedAction>,    // dry-run plan log
    pub overlay: SimState,           // simulated host/state for dry-run reads
    pub comps: Vec<Compensation>,    // active transaction's inverse stack
    pub secrets: SecretRegistry,     // provenance-tagged values for redaction
    pub state: StateHandle,          // locked, atomically-flushed state
    pub ssh: SshConfig,
    pub trace: bool,                 // NRG_TRACE
}
```

### 3.4 Execution & SSH transport (corrected by inventory)

The async runner (`task_runner.rs` / `ssh_command::build_process`) already uses
**argv + stdin** transport (`ssh host 'bash -se'`, script written to stdin), **not**
the EOF-heredoc string in `build_ssh_command` (that path is display-only in `run`'s
pretend mode). So there is **no hidden divergence** between the two SSH paths, and the
exec-mode `ssh_exec` (`Command::new("ssh").arg(host).arg(cmd)`) and the run-mode
streaming path can share one transport. The unified model keeps the streaming runner as
the executor that builtins call.

## 4. Verified Rhai integration (rhai 1.25.1)

Every mechanism below was compiled and run by the verification workflow; these are the
load-bearing APIs:

- **Native fn + captured `Arc<Mutex<RunCtx>>`:** `engine.register_fn("ssh_exec", move |h:&str,c:&str| -> ExecResult { … })`.
- **Custom type + getters:** `register_type_with_name::<ExecResult>("ExecResult").register_get("ok", |r:&mut ExecResult| r.ok)…` — read-only (no setter ⇒ `r.ok = x` is rejected).
- **Compile + run + call:** `compile_file(path.into())` → `run_ast(&ast)` (module top-level) and `call_fn::<Dynamic>(&mut scope, &ast, "deploy", (arg,))`; `CallFnOptions::new().eval_ast(false)` to call without re-running top-level.
- **Compensation callback (the critical one):** accept `rhai::FnPtr` as a fn param, push to `Vec<FnPtr>`, later invoke from Rust during unwind via `fp.call::<()>(&engine, &ast, ())`. **Empty AST suffices** when the closure body calls only global builtins (our case); retain the defining AST only if a closure calls script-defined fns. Verified: a stored `on_rollback(|| ssh_exec(...))` closure executed *after* its originating eval finished and appended to the shared plan log.
- **Errors:** native fns return `Result<T, Box<EvalAltResult>>`; `"msg".into()` ⇒ `ErrorRuntime`; structured `ErrorRuntime(Dynamic::from_map(m), Position::NONE)` is catchable in-script via `try { } catch(e) { e.kind }`.
- **Modules:** `engine.set_module_resolver(FileModuleResolver::new_with_path("<root>"))`; `import "lib/docker" as docker;` resolves to `<root>/lib/docker.rhai`. **Global builtins are in scope inside imported module fns** — verified (`ssh_exec` ran from within `docker::pull`). **⇒ stdlib files must use the `.rhai` extension.**
- **Safety limits:** `set_max_operations(0)` = unlimited (trusted scripts); `on_progress(|ops| …)` is the host kill-switch (ties to a tokio cancel token) even when unlimited.

**Blockers folded in:** (a) `sync` feature required; (b) `.rhai` extension required; (c) interior mutability required; (d) tag is not per-call state; (e) `FnPtr::call` needs `&Engine` + `&AST`. None block the design.

## 5. Execution model & secret delivery (behavior change)

**Secrets are no longer exported as remote environment variables.** Today
`build_script` emits `export KEY="value"`, which is readable on the remote host via
`/proc/<pid>/environ` and `ps eww` by any co-tenant — `nrg`-side redaction cannot fix a
leak that lives on the remote box. New model:

- Non-secret env vars: still exported (they're not sensitive).
- Secret env vars: delivered to the remote process via **stdin/`--env-file`/tmpfs
  (`0600`, removed after use)**, never as `-e KEY=secret` (also visible in
  `docker inspect`) and never on a command line. For containers prefer
  `--env-file <(…)` / BuildKit `--secret`.
- A `with_secret(|| …)` script block suspends the `DEBUG` trace trap around
  secret-handling commands.

## 6. Dry-run (effect interception + simulated overlay)

Effect classification is **by builtin, never by parsing the command string** (a free-form
`ssh_exec(host, container_cmd()+" pull "+tag)` looks like a read but mutates). Rules:

- **Mutating builtins** (`ssh_exec`, `local_exec`, `upload`, `write_remote`,
  `state_set`, and every stdlib container/proxy wrapper) → in `DryRun`: record a
  `PlannedAction`, update the **simulated overlay**, return a synthetic `ok=true`
  result. Do **not** execute.
- **Read builtins** are an explicit allowlist: a new **`ssh_probe(host, cmd)`** (declared
  read-only), `state_get`, and `http_get`. `ssh_exec` is **never** read-only; stdlib
  reads (`docker_container_running`, `inspect`, `_pick_port`'s `nc` probe) must route
  through `ssh_probe`/declared-effect wrappers.
- **Simulated overlay:** an in-memory model seeded from real state, updated by recorded
  stubbed writes, so reads-after-writes stay consistent. In `DryRun`,
  `docker_container_running`/`inspect`/`_pick_port`/`state_get` read the overlay (not the
  live host); `wait_healthy`/`wait_port`/`http_get` **short-circuit to healthy**. Ports
  print as symbolic `<auto>` rather than a number a real run won't reproduce.
- **Output:** end-of-run plan log — `N actions, M hosts, 0 executed` — with secrets
  redacted.

This is the single biggest deepening from the original sketch: "return ok=true" alone
diverges on every state-dependent branch (health-checking a container that was never
started), so the overlay + read-stubbing is **required**, not optional.

## 7. Transactions (compensation stack)

Builtins: `transaction(|| { … })` and `on_rollback(|| { … })`. Hardened semantics from
adversarial review (all **must-fix**):

1. **Register-inverse-before-effect.** Push the compensation, *then* perform the side
   effect; the inverse must tolerate "effect never happened" (idempotent).
2. **Best-effort, error-isolated unwind.** On `throw`, run the LIFO stack with each
   compensation wrapped in catch+log; **never abort the unwind** because one
   compensation failed. Aggregate failures and report at the end.
3. **Never destroy the rollback target inside the window.** `deploy_to_host` is
   **reordered**: the old container stays running + named until *after* the transaction
   commits (post-deploy hooks + health all green). Proxy-switch compensation restores the
   **snapshotted prior proxy target** (a guaranteed-alive old container), not a
   reconstructed name. Old-container `rename`/`remove` moves to a **post-commit,
   non-transactional cleanup** step.
4. **Idempotent compensations** (`rm -f`, `stop-if-running`, `|| true`); capture
   referenced data by value at registration.
5. **Fan-out:** `ssh_exec_all` currently swallows per-host failures; inside a transaction
   it must surface them so each failed host triggers only its own inverse.
6. **Nested/rolling deploys:** flatten to one fleet-wide stack (savepoints, not
   independent inner commits) **or** explicitly document rolling deploys as non-atomic and
   emit a **per-host committed-version manifest** so an operator can finish/abort
   manually. Decision: **flatten** for `deploy()`; document the escape hatch.

This also repairs the **broken rollback wiring** found in review (`deploy()` never set
`.rollback_image`; `rollback()` read it): the transaction snapshots `.prev` image under
the state lock before switching.

## 8. State (locking, atomicity, corruption)

- **Atomic writes:** write `state.json.tmp` → `fsync` → `rename`; keep a
  `state.json.bak`. Add a schema `version` + checksum.
- **Corruption is fatal, not silent.** Replace `serde_json::from_str(...).unwrap_or_default()`:
  a **missing** file ⇒ empty (legitimate); a **present-but-unparseable** file ⇒
  hard-error and abort (today it silently zeroes all deploy history).
- **Project root** resolved from an explicit marker (`.energize/` dir or
  `energize.toml`), **not `.git`**, with **no fallback above `$HOME`**; refuse to run if
  no marker (don't guess, don't plant `.energize` at `/`).
- **Locking:** OS advisory `flock` (auto-released on crash) keyed on the
  **canonicalized real path** (so symlink aliases share one lock). A **mutating run**
  (`run`/`exec`, non-dry) takes an **exclusive** lock for its duration — concurrent
  deploys *should* serialize. **Reads** (`nrg tasks`/status / dry-run) read the
  atomically-published snapshot without taking the exclusive lock (shared lock or
  lock-free read), so a 10-minute deploy never blocks a cheap read. **Re-entrant** for
  nested `nrg` invocations via an inherited env token. **Detect NFS and warn/refuse**
  (advise a local state dir).

## 9. Secrets (full bundle + provenance)

- **`Secret` type** (provenance-tagged) threaded end-to-end. Redaction happens **at
  interpolation time**, covering known encodings (base64/url/json), not a post-hoc
  substring scan (which both misses transformed secrets and false-positives on short
  values).
- **Reject too-short secrets** at definition; **forbid `state_set` of a `Secret`** (keep
  plaintext out of `state.json`); **keep secrets out of URLs** (use headers; redact
  userinfo/query in trace).
- **`sh_quote()`** implements the canonical POSIX algorithm: replace each `'` with
  `'\''` and wrap in single quotes; **handles newlines** (the current `escape_value`
  does not, breaking multiline secrets). Unit-tested with embedded quotes, newlines, `$`.
  Used throughout the stdlib so env/volume/port values can't break or inject.
- **`DEBUG` trap only when `NRG_TRACE`** (today it's unconditional and echoes
  `$BASH_COMMAND` — every command, with expanded args — to stderr). `with_secret`
  suspends it.
- **Redact** `Secret` values from plan log and trace.

## 10. Stdlib port

**Actual files on disk** (README's `nginx`/`tls`/`provision` do **not** exist — scope is
smaller than advertised): `runtime`, `docker`, `proxy`, `healthcheck`, `registry`,
`deploy`, and examples `Energize`, `django`, `laravel`, `nextjs`, `phoenix`, `rails`.

**Port order** (dependency-respecting): `runtime` → `docker` → `proxy` → `healthcheck`
→ `registry` → `deploy` → examples. `deploy` is the only **tricky** module (transaction
wrapping + reordering + state); the rest are **mechanical**.

**Conventions:**
- No kwargs/defaults in Rhai ⇒ optional args become a **config object map**:
  `deploy(hosts, image, service, #{ container_port: 3000, health_path: "/up" })` with
  `cfg.get("container_port")`-style reads + defaults.
- `load(...)` → `import "lib/x" as x;` (files renamed `.rhai`).
- `fail(msg)` → `throw msg`.

**Corrected Starlark→Rhai cheat-sheet** (the survey's draft contained Rust-isms like
`println!`/`.iter()` — these are the real Rhai forms; each is verified against the Rhai
book during the port, and any gap in Rhai's array/string stdlib is filled with a
host-registered helper):

| Starlark | Rhai (script) |
|---|---|
| `print(x)` | `print(x)` |
| `[r for r in rs if not r.ok]` | `rs.filter(\|r\| !r.ok)` |
| `for k, v in d.items():` | `for k in d.keys() { let v = d[k]; … }` |
| `def f(a, b=1, c="x"):` | `fn f(a, cfg) { let b = if cfg.contains("b") { cfg.b } else { 1 }; … }` |
| `f(x=1, y=2)` | `f(#{ x: 1, y: 2 })` |
| `" && ".join(parts)` | host `join(parts, " && ")` helper (Rhai core has no `join`) |
| `s.strip()` | `s.trim()` |
| `s.split(":")[i]` | `s.split(":")[i]` (returns array) |
| `"k" in d` | `d.contains("k")` |
| `str(n)` | `n.to_string()` |
| `range(n)` | `0..n` |
| `fail(m)` | `throw m` |

## 11. Testing strategy (TDD)

Delete the ~290 Starlark/bash parser tests. Add, **test-first**, behind a
**trait-injected command runner** so SSH/local exec is faked:

- `sh_quote`/escaping edge cases (embedded `'`, newline, `$`, backtick).
- Dry-run: a fake runner asserts **zero** real side effects; overlay consistency
  (read-after-stubbed-write); `wait_healthy` short-circuits; plan-log contents.
- Transactions: inject a `throw` on host 3 of 5 → assert LIFO best-effort unwind,
  a throwing compensation doesn't abort the rest, old container survives until commit,
  proxy restored to snapshotted target.
- State: atomic-write crash safety (kill between tmp and rename → old state intact),
  hard-fail on corrupt file, lock contention (second exclusive blocks; read doesn't),
  re-entrancy.
- Secrets: redaction across encodings; reject short secret; `state_set(Secret)` errors;
  no secret in argv/environ of the faked command.
- Rhai integration smoke: `ssh_exec`/`ExecResult` getters, `import`, `call_fn`,
  `on_rollback` callback executes.

## 12. Phased execution plan

Each phase ends green (`cargo test` + `cargo clippy`) with a verification checkpoint.

- **P0 — Engine skeleton.** Cargo swap; `RunCtx`; `ExecResult`/`HttpResponse` Rhai types; core builtins (`ssh_exec`, `ssh_probe`, `local_exec`, `http_get/post`, `sleep`, `nrg_env`, `env_or`, `print`) in **Live** mode; `FileModuleResolver`; `exec`/`run` CLI over one engine. A trivial `Energize.rhai` runs end-to-end.
- **P1 — State.** Project-root marker, `fd-lock`, atomic+fsync+rename, corruption hard-fail, schema/backup; `state_*` builtins on the locked handle.
- **P2 — Secrets.** `Secret` type + registry + redaction; `sh_quote`; trap gated on `NRG_TRACE`; secret delivery via stdin/env-file (no remote env export); `with_secret`.
- **P3 — Dry-run.** Effect classification, plan log, simulated overlay, read-stubbing, `--dry-run` flag.
- **P4 — Transactions.** `transaction`/`on_rollback`; best-effort unwind; fan-out per-host failure surfacing.
- **P5 — Stdlib port.** `runtime`→…→`deploy` + examples, in order; `deploy` reordered for the rollback window and wrapped in `transaction`; `.prev` snapshot fixes rollback wiring.
- **P6 — Cleanup.** Delete Starlark/bash/`loader`; remove dead deps; rewrite README/`init`/`doctor`; drop the README's non-existent `nginx`/`tls`/`provision` claims (or mark as roadmap).

## 13. Non-goals

Two-phase plan/apply (Terraform-style); a full-screen TUI; Starlark/bash back-compat;
a Rhai type-checker (Rhai is dynamically typed — accepted); implementing the
nonexistent `nginx`/`tls`/`provision` modules in this migration.

## 14. Accepted tradeoffs / residual risk

- **Dry-run is a simulation, not a proof.** The overlay models known effects; a deploy
  whose behavior depends on un-modeled remote state can still diverge. Plan output says
  so.
- **Rolling deploy is "flattened-atomic," not distributed-atomic.** A mid-fleet failure
  unwinds touched hosts best-effort; a compensation that genuinely can't run is logged,
  and the per-host manifest lets an operator finish by hand.
- **Dynamic typing remains** — config errors surface at eval time, not compile time
  (mitigated by `doctor`-time smoke evaluation of `Energize.rhai`).
