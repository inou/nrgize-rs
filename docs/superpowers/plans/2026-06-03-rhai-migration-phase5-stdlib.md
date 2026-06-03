# Rhai Migration — Phase 5: Stdlib Port (fleet-atomic) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Port the 6 Starlark stdlib modules + 6 examples to `.rhai`, close the accumulated
engine debts (off-argv secret stdin channel; dry-run container overlay), implement a
**true fleet-atomic** `deploy()`, and unify `nrg run <fn>` onto the engine.

**Scale:** This is ~4× a normal phase. It is split into sub-chunks, each its own green commit
set with TDD + an adversarial review at the end of the engine work and again after `deploy()`:

- **P5a — engine extensions** (Rust): (1) stdin channel + `write_remote`; (2) `SimState` +
  `sim_*` mode-aware container builtins; (3) a `join` helper + `is_dry_run()`.
- **P5b — stdlib port** (`.rhai`): `runtime` → `docker` → `proxy` → `healthcheck` → `registry`.
- **P5c — fleet-atomic `deploy()`** + `rollback()` + `accessory_run()`.
- **P5d — examples** (rails/django/nextjs/phoenix/laravel/setup) + **`nrg run <fn>`** unify.

## Key design decisions (from the P5 design workflow)

### Container overlay (dry-run fidelity)
`RunCtx` gains `sim: Arc<Mutex<SimState>>` (containers/ports/health per host). The ported stdlib
does container reads/mutations ONLY through typed `sim_*` builtins. In **Live** each runs the
real command via the runner; in **DryRun** each updates the sim, records a `PlannedAction`, and
returns a synthetic result (reads lazily seed from ONE real probe per entity). So a stubbed
`sim_docker_run` makes `sim_container_running(new)` true and `sim_container_healthy(new)` true —
the deploy dry-run doesn't diverge. Builtins: `is_dry_run`, `sim_container_running`,
`sim_image_id`, `sim_pick_port`, `sim_docker_run/stop/rename/remove`, `sim_proxy_switch`,
`sim_wait_port`, `sim_container_healthy`. **Contract:** stdlib must never inline a raw
`docker inspect`/`nc -z` via `ssh_exec` — that bypasses the sim and diverges.

### Stdin channel (off-argv secrets)
`CommandRunner` gains `run_ssh_stdin(host, cmd, stdin)` + `run_local_stdin(cmd, stdin)`
(`RealRunner` pipes to the child's stdin; `FakeRunner` records the stdin separately). Builtins
`ssh_exec_stdin`, `local_exec_stdin`, `write_remote(host, content, path)` (`umask 077; cat >
'path'`). **Contract:** passwords/tokens/env-file bodies are **stdin-only** via these; server/
user/URLs may stay on argv. `registry_login` becomes `<cmd> login <server> -u <user>
--password-stdin` with `reveal(password)` on stdin (already redaction-registered by `secret()`).
`ecr_login` keeps its `aws … | … --password-stdin` pipeline (secret never crosses argv). The
stdin builtins do **not** auto-register the payload as a secret (would over-redact a non-secret
config body) — they rely on `secret()` having registered real secrets; trace shows the redacted
cmd + stdin **byte-length only**.

### Fleet-atomic deploy()
`build → push → pull-all` run OUTSIDE the transaction (idempotent, no live-traffic state). Then
ONE flattened `transaction(|| { for host in hosts { deploy_one(host) } })`. Per host: capture
`old_target` by value; `sim_docker_run` the NEW container under a unique name (old keeps running
under its unchanged name); `wait_healthy(new)`; **register `on_rollback(restore proxy→old)` and
`on_rollback(rm -f new)` BEFORE** `sim_proxy_switch(new)`. Per-host failure ⇒ `throw` ⇒
fleet-wide LIFO unwind (every touched host's proxy restored + new removed; **no old container
was ever destroyed**). On success the transaction returns Ok ⇒ **post-commit cleanup pass**
(OUTSIDE the txn) removes every old container + `state_set(<svc>.version/.image)`. `<svc>.prev`
is snapshotted from the current `.image` UNDER LOCK BEFORE the deploy (fixes the broken rollback
wiring). Compensations are idempotent (`rm -f … || true`). The rolling switch uses sequential
per-host `ssh_exec` (NOT `ssh_exec_all`, which swallows per-host failures).

### Module-global state caveat
Rhai `import` yields a FRESH module instance per import, so the legacy `_RUNTIME` mutable
module-global dict does NOT survive. `runtime.rhai` backs the runtime choice with the process-
global StateStore: `set_runtime(x)` → `state_set("nrg.runtime.cmd", …)`; `container_cmd()` →
`state_get("nrg.runtime.cmd")` defaulting to `"docker"`.

---

## P5a.1: Stdin channel + write_remote (THIS chunk)

**Files:** `src/engine/runner.rs`, `src/engine/builtins/exec.rs`, `src/engine/plan.rs` (doc)

### Task 1: CommandRunner stdin methods

- [ ] **Step 1: Extend the trait + RealRunner + FakeRunner** in `src/engine/runner.rs`.

Add to `trait CommandRunner`:

```rust
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput;
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput;
```

Add a private helper + impls on `RealRunner` (write stdin, close, then read — for the small
payloads we use, write-before-read is safe; documented):

```rust
fn piped(mut command: std::process::Command, stdin: &str) -> RawOutput {
    use std::io::Write;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return RawOutput { stdout: String::new(), stderr: format!("spawn failed: {e}"), exit_code: -1 },
    };
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(stdin.as_bytes());
        // drop closes the pipe (EOF) so the child can finish reading
    }
    match child.wait_with_output() {
        Ok(o) => RawOutput {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            exit_code: o.status.code().unwrap_or(-1) as i64,
        },
        Err(e) => RawOutput { stdout: String::new(), stderr: format!("wait failed: {e}"), exit_code: -1 },
    }
}
```

In `impl CommandRunner for RealRunner`:

```rust
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        let resolved = self.ssh.resolve_host(host);
        let mut c = Command::new("ssh");
        c.args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=10"])
            .arg(&resolved)
            .arg(cmd);
        piped(c, stdin)
    }
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        piped(c, stdin)
    }
```

In `#[cfg(test)] impl CommandRunner for FakeRunner` (record cmd AND stdin, distinct prefix):

```rust
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("ssh-stdin {host}: {cmd} <<< {stdin}"));
        self.default.clone()
    }
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("local-stdin: {cmd} <<< {stdin}"));
        self.default.clone()
    }
```

- [ ] **Step 2: Test** in `runner.rs` tests:

```rust
    #[test]
    fn fake_runner_records_stdin_separately() {
        let r = FakeRunner::new();
        r.run_ssh_stdin("web1", "docker login -u u --password-stdin", "topsecret");
        assert_eq!(r.calls(), vec!["ssh-stdin web1: docker login -u u --password-stdin <<< topsecret".to_string()]);
    }
```

- [ ] **Step 3:** `cargo test --bin nrg engine::runner` → PASS. Commit:

```bash
git add src/engine/runner.rs
git commit -m "feat(engine): CommandRunner stdin channel (run_ssh_stdin/run_local_stdin)"
```

### Task 2: ssh_exec_stdin / local_exec_stdin / write_remote builtins

- [ ] **Step 1: Add to `src/engine/builtins/exec.rs`** (after `ssh_exec_all`). Each mirrors the
mutating-builtin shape: snapshot; trace prints the REDACTED cmd + `stdin <N bytes>` (never the
payload); dry-run records (the cmd, via `record` which redacts) and returns synthetic ok; live
calls the stdin runner method.

```rust
    // ssh_exec_stdin(host, cmd, stdin) — MUTATING; delivers `stdin` off-argv (e.g. a password
    // to `docker login --password-stdin`). The payload is NEVER traced or put on argv.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec_stdin", move |host: &str, cmd: &str, stdin: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_exec_stdin {host} -> {} (stdin {} bytes)", traced(cmd, &snap), stdin.len());
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("ssh-stdin", Some(host), cmd.to_string());
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: host.into() };
            }
            to_result(host, snap.runner.run_ssh_stdin(host, cmd, stdin))
        });
    }
    // local_exec_stdin(cmd, stdin) — MUTATING local mirror.
    {
        let ctx = ctx.clone();
        engine.register_fn("local_exec_stdin", move |cmd: &str, stdin: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] local_exec_stdin -> {} (stdin {} bytes)", traced(cmd, &snap), stdin.len());
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("local-stdin", None, cmd.to_string());
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: String::new() };
            }
            to_result("", snap.runner.run_local_stdin(cmd, stdin))
        });
    }
    // write_remote(host, content, remote_path) — MUTATING; writes content to a 0600 remote file
    // via the stdin channel (content never on argv). For secret env-files etc.
    {
        let ctx = ctx.clone();
        engine.register_fn("write_remote", move |host: &str, content: &str, remote_path: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            let cmd = format!("umask 077; cat > {}", crate::engine::secret::posix_quote(remote_path));
            if snap.trace {
                eprintln!("[nrg] write_remote {host} -> {remote_path} ({} bytes)", content.len());
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("write", Some(host), format!("write {} bytes -> {remote_path}", content.len()));
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: host.into() };
            }
            to_result(host, snap.runner.run_ssh_stdin(host, &cmd, content))
        });
    }
```

- [ ] **Step 2: Tests** in `exec.rs` tests (live path keeps stdin off argv; dry-run records;
secret redacted in trace path is already covered):

```rust
    #[test]
    fn ssh_exec_stdin_keeps_payload_off_argv() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        e.run(r#"ssh_exec_stdin("web1", "docker login -u u --password-stdin", "topsecretpw");"#).unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("ssh-stdin web1: docker login -u u --password-stdin"));
        // payload present in the stdin slot (after <<<), not on the argv portion:
        let (argv, _stdin) = calls[0].split_once("<<<").unwrap();
        assert!(!argv.contains("topsecretpw"));
    }

    #[test]
    fn write_remote_uses_stdin_not_argv() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        e.run(r#"write_remote("web1", "SECRET=abc123", "/run/app.env");"#).unwrap();
        let calls = fake.calls();
        assert!(calls[0].contains("umask 077; cat > '/run/app.env'"));
        assert!(calls[0].contains("<<< SECRET=abc123")); // content on stdin
        assert!(!calls[0].split("<<<").next().unwrap().contains("abc123")); // not on argv
    }

    #[test]
    fn write_remote_records_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let e = engine_with(ctx.clone());
        e.run(r#"write_remote("web1", "BIG=body", "/run/app.env");"#).unwrap();
        assert!(fake.calls().is_empty());
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert_eq!(plan[0].kind, "write");
        assert!(plan[0].detail.contains("/run/app.env"));
    }
```

- [ ] **Step 3:** `cargo test --bin nrg engine::builtins::exec` → PASS. `cargo clippy
--all-targets 2>&1 | grep src/engine` → empty. Update `plan.rs` `kind` doc comment to mention
`ssh-stdin`/`local-stdin`/`write`. Commit:

```bash
git add src/engine/builtins/exec.rs src/engine/plan.rs
git commit -m "feat(engine): ssh_exec_stdin/local_exec_stdin/write_remote (off-argv secrets)"
```

---

## P5a.2 / P5b / P5c / P5d

Elaborated into full TDD tasks as each chunk is reached (the design above is the spec). P5a.2 =
`sim.rs` + `sim_*` builtins + `join` helper + `is_dry_run`. P5b = port the 5 lib modules. P5c =
`deploy.rhai` (fleet-atomic). P5d = examples + `nrg run <fn>` via `call_fn`. An adversarial
review runs after P5a (engine extensions) and after P5c (deploy).

---

## Phase 5 build + review outcome (workflow-built, adversarially reviewed, 2026-06-03)

P5a.2–P5d built by a 4-agent sequential workflow (sim engine → stdlib → deploy → examples+unify),
then independently verified (180 tests green, a real 2-host deploy `--dry-run` produces a correct
fleet-atomic plan) and adversarially reviewed (4 lenses + verify).

**Verified sound:** stdlib Rhai gotchas clean (`trim()`/`make_lower()` only in statement position;
`join()` builtin used; no map `.items()`; bool-only conditions); `registry_login` password is
`--password-stdin`-only, never on argv; state-backed runtime choice works across fresh module
instances; sim-routing intact (no raw `docker inspect`/`nc -z`); sim read-after-write consistent;
sim lock discipline deadlock-free; the single fleet transaction + LIFO unwind correct.

**Fixed (review + my own catch):**
- (mine) rollback compensation order inverted → blackhole; swapped.
- **HIGH** rm-new registered after the health wait → a health failure leaked the new container;
  now registered right after `docker_run`.
- **HIGH** `wait_container_healthy` (needs a Docker HEALTHCHECK) → removed; HTTP `wait_healthy` only.
- **MEDIUM** `state_set(port)` inside the txn → corrupted the next deploy's `old_target`; moved to
  post-commit.
- **LOW** macOS-unsafe `date +%s%N` name → port-based deterministic unique name.
- **CRITICAL** `nrg run <typo>` ran the whole top-level deploy then said "not found" → `run_fn`
  now refuses a missing function BEFORE running anything (regression test added).

**Deferred (carry into P6 / a follow-up):**
- **HIGH (podman/nerdctl only)** the sim's Rust seeding/Live probes hardcode `docker` (the
  mutation path honors `container_cmd()`). For non-docker runtimes a LIVE running/health probe
  mis-reads. Default docker is correct; removing `wait_container_healthy` defused the worst case.
  Fix by threading `state_get("nrg.runtime.cmd")` into the sim probes.
- **P6** `nrg run` dispatch vs the legacy Starlark `--var`/`--pretend` flags: resolved when P6
  deletes the Starlark run path; give the Rhai `nrg run` a real `--dry-run` flag there.
- **P6 docs** `nrg run` args are strings (int params need coercion); examples need `lib/` vendored
  as a sibling when copied to a project root — document both in the README rewrite.

## Self-review (author)

- **Spec §9/§6/§7 + P2/P3/P4 debts coverage:** stdin channel (P2) → P5a.1; container overlay
  (P3) → P5a.2; fleet-atomic deploy reorder (P4/§7) → P5c. **Deferred within P5:** the
  `local_probe` read-class builtin for `_auto_detect` under dry-run (runtime auto-detect runs
  eagerly at config time, outside dry-run, so deferred); `--env-file` process-substitution needs
  bash not `sh` (use `write_remote` to a tmpfs file instead — already the contract).
- **Placeholders:** none in P5a.1. **Types:** `run_ssh_stdin`/`run_local_stdin`,
  `ssh_exec_stdin`/`local_exec_stdin`/`write_remote`, `posix_quote` — consistent.
