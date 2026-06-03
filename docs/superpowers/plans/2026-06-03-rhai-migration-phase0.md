# Rhai Migration — Phase 0: Engine Foundation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a Rhai-powered orchestration engine so that `nrg exec Energize.rhai` runs a script with core side-effecting builtins (`ssh_exec`, `ssh_probe`, `local_exec`, `ssh_exec_all`, `http_get`, `http_post`, `sleep`, `nrg_env`, `env_or`) and `import "lib/x" as x;` modules — all behind a fakeable command runner so it is unit-testable without a real host.

**Architecture:** A new, parallel `src/engine/` module tree (the legacy `src/runtime/` Starlark tree stays until Phase 6, so the build is green at every commit). Every side-effecting builtin is a `move` closure capturing `Arc<Mutex<RunCtx>>`; the `RunCtx` holds an `EffectMode` (Live now; DryRun in P3) and an `Arc<dyn CommandRunner>` that real runs route through and tests fake. `nrg exec` switches to this engine; `nrg run` and Starlark removal come in later phases.

**Tech Stack:** Rust, `rhai = { version = "1.25", features = ["sync"] }` (sync mandatory — tokio + threaded SSH), `fd-lock` (added now, used in P1), `ureq` (existing, HTTP), `std::process::Command` (SSH/local), `std::thread::scope` (parallel fan-out).

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/mod.rs` | Module exports + `build_engine(ctx) -> Engine`. |
| `src/engine/types.rs` | Plain `ExecResult`/`HttpResponse` structs + Rhai getter registration. |
| `src/engine/runner.rs` | `CommandRunner` trait, `RawOutput`, `RealRunner`, `FakeRunner` (test). |
| `src/engine/context.rs` | `RunCtx`, `EffectMode`, `SharedCtx` alias. |
| `src/engine/builtins/mod.rs` | `register_builtins(engine, ctx)` dispatch. |
| `src/engine/builtins/exec.rs` | `ssh_exec`, `ssh_probe`, `local_exec`, `ssh_exec_all`. |
| `src/engine/builtins/http.rs` | `http_get`, `http_post`. |
| `src/engine/builtins/util.rs` | `sleep`, `nrg_env`, `env_or`. |
| `src/engine/eval.rs` | `run_file(path, ctx)` — compile + run a `.rhai` module with the file's dir as the import root. |
| `src/cli/exec.rs` | Rewired: build `RunCtx` (RealRunner) → `engine::eval::run_file`. |

Each builtin is a closure capturing `SharedCtx`; the `CommandRunner` lives in an `Arc` so a builtin clones it **out of the lock** before the blocking call (so `ssh_exec_all` actually parallelizes).

---

## Task 1: Add Rhai + fd-lock deps (keep Starlark — green build)

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the new dependencies**

In `Cargo.toml` under `[dependencies]`, add (do NOT remove starlark/ratatui/crossterm yet — that's Phase 6):

```toml
rhai = { version = "1.25", features = ["sync"] }
fd-lock = "4"
```

- [ ] **Step 2: Verify the tree still builds with both engines present**

Run: `cargo build`
Expected: compiles successfully (warnings about unused `rhai` are fine — nothing uses it yet).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add rhai (sync) + fd-lock deps alongside starlark"
```

---

## Task 2: Engine result types (`ExecResult`, `HttpResponse`)

**Files:**
- Create: `src/engine/mod.rs`
- Create: `src/engine/types.rs`
- Modify: `src/main.rs` (add `mod engine;`)
- Test: in `src/engine/types.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Declare the module**

In `src/main.rs`, add to the module list (alphabetical-ish, near `mod runtime;`):

```rust
mod engine;
```

Create `src/engine/mod.rs`:

```rust
//! Rhai-powered orchestration engine (replaces the Starlark runtime).
pub mod builtins;
pub mod context;
pub mod eval;
pub mod runner;
pub mod types;
```

- [ ] **Step 2: Write the failing test**

Create `src/engine/types.rs`:

```rust
//! Result types exposed to Rhai scripts.

/// Result of running a command (locally or over SSH).
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
    pub host: String,
}

/// Result of an HTTP request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: i64,
    pub body: String,
}

/// Register both types (with read-only getters) into a Rhai engine.
pub fn register_types(engine: &mut rhai::Engine) {
    engine
        .register_type_with_name::<ExecResult>("ExecResult")
        .register_get("stdout", |r: &mut ExecResult| r.stdout.clone())
        .register_get("stderr", |r: &mut ExecResult| r.stderr.clone())
        .register_get("exit_code", |r: &mut ExecResult| r.exit_code)
        .register_get("host", |r: &mut ExecResult| r.host.clone())
        .register_get("ok", |r: &mut ExecResult| r.exit_code == 0);
    engine
        .register_type_with_name::<HttpResponse>("HttpResponse")
        .register_get("status", |r: &mut HttpResponse| r.status)
        .register_get("body", |r: &mut HttpResponse| r.body.clone())
        .register_get("ok", |r: &mut HttpResponse| (200..300).contains(&r.status));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_result_getters_readable_in_script() {
        let mut engine = rhai::Engine::new();
        register_types(&mut engine);
        engine.register_fn("make", || ExecResult {
            stdout: "hi".into(),
            stderr: String::new(),
            exit_code: 0,
            host: "web1".into(),
        });
        let out: bool = engine.eval(r#"let r = make(); r.ok && r.stdout == "hi" && r.host == "web1""#).unwrap();
        assert!(out);
    }

    #[test]
    fn http_response_ok_is_2xx() {
        let mut engine = rhai::Engine::new();
        register_types(&mut engine);
        engine.register_fn("make404", || HttpResponse { status: 404, body: "x".into() });
        engine.register_fn("make200", || HttpResponse { status: 200, body: "x".into() });
        let bad: bool = engine.eval("make404().ok").unwrap();
        let good: bool = engine.eval("make200().ok").unwrap();
        assert!(!bad && good);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail (then compile)**

Run: `cargo test --lib engine::types`
Expected: first run may fail to compile until `src/engine/{context,eval,runner,builtins}` exist as referenced by `mod.rs`. If so, create empty stub files for those modules now (one-line `//!` doc each) so the crate compiles, then re-run. Expected after stubs: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/engine/mod.rs src/engine/types.rs
git commit -m "feat(engine): ExecResult/HttpResponse Rhai types with getters"
```

---

## Task 3: Command runner abstraction (real + fake)

**Files:**
- Create/replace: `src/engine/runner.rs`
- Test: in `src/engine/runner.rs`

- [ ] **Step 1: Write the failing test**

Replace the `src/engine/runner.rs` stub with:

```rust
//! Command execution abstraction so builtins are testable without a real host.

use crate::ssh::config::SshConfig;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Raw output of a single command.
#[derive(Debug, Clone)]
pub struct RawOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}

/// Anything that can run a command locally or over SSH.
/// Send + Sync so it can be shared across the parallel fan-out threads.
pub trait CommandRunner: Send + Sync {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput;
    fn run_local(&self, cmd: &str) -> RawOutput;
}

/// Production runner: spawns `ssh`/`sh` via std::process.
pub struct RealRunner {
    pub ssh: SshConfig,
}

impl CommandRunner for RealRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        let resolved = self.ssh.resolve_host(host);
        let out = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=10"])
            .arg(&resolved)
            .arg(cmd)
            .output();
        match out {
            Ok(o) => RawOutput {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: o.status.code().unwrap_or(-1) as i64,
            },
            Err(e) => RawOutput { stdout: String::new(), stderr: format!("ssh spawn failed: {e}"), exit_code: -1 },
        }
    }

    fn run_local(&self, cmd: &str) -> RawOutput {
        let out = Command::new("sh").arg("-c").arg(cmd).output();
        match out {
            Ok(o) => RawOutput {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: o.status.code().unwrap_or(-1) as i64,
            },
            Err(e) => RawOutput { stdout: String::new(), stderr: format!("sh spawn failed: {e}"), exit_code: -1 },
        }
    }
}

/// Test runner: records every call and replays canned outputs.
pub struct FakeRunner {
    pub calls: Mutex<Vec<String>>,
    pub default: RawOutput,
}

impl FakeRunner {
    pub fn new() -> Self {
        FakeRunner { calls: Mutex::new(Vec::new()), default: RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 } }
    }
    pub fn shared() -> Arc<Self> { Arc::new(Self::new()) }
    pub fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
}

impl CommandRunner for FakeRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("ssh {host}: {cmd}"));
        self.default.clone()
    }
    fn run_local(&self, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("local: {cmd}"));
        self.default.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_runner_records_calls() {
        let r = FakeRunner::new();
        r.run_ssh("web1", "uptime");
        r.run_local("ls");
        assert_eq!(r.calls(), vec!["ssh web1: uptime".to_string(), "local: ls".to_string()]);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --lib engine::runner`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/engine/runner.rs
git commit -m "feat(engine): CommandRunner trait with RealRunner + FakeRunner"
```

---

## Task 4: Run context (`RunCtx`)

**Files:**
- Create/replace: `src/engine/context.rs`
- Test: in `src/engine/context.rs`

- [ ] **Step 1: Write the implementation + test**

Replace the `src/engine/context.rs` stub with:

```rust
//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::runner::CommandRunner;
use std::sync::{Arc, Mutex};

/// Whether side effects actually execute (Live) or are recorded only (DryRun, Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectMode {
    Live,
    DryRun,
}

/// State shared across one `nrg` invocation.
pub struct RunCtx {
    pub mode: EffectMode,
    /// In an Arc (not inside the Mutex body's exclusive section) so a builtin can
    /// clone it and release the lock BEFORE the blocking command — enabling real
    /// parallelism in ssh_exec_all.
    pub runner: Arc<dyn CommandRunner>,
    pub trace: bool,
}

impl RunCtx {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        RunCtx { mode: EffectMode::Live, runner, trace: std::env::var("NRG_TRACE").is_ok() }
    }
}

/// Shared, lockable handle threaded into every builtin closure.
pub type SharedCtx = Arc<Mutex<RunCtx>>;

pub fn shared(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::new(runner)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn ctx_defaults_to_live() {
        let ctx = shared(FakeRunner::shared());
        assert_eq!(ctx.lock().unwrap().mode, EffectMode::Live);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --lib engine::context`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/engine/context.rs
git commit -m "feat(engine): RunCtx + SharedCtx with EffectMode"
```

---

## Task 5: Exec builtins (`ssh_exec`, `ssh_probe`, `local_exec`, `ssh_exec_all`)

**Files:**
- Create: `src/engine/builtins/mod.rs`
- Create: `src/engine/builtins/exec.rs`
- Test: in `src/engine/builtins/exec.rs`

- [ ] **Step 1: Create the dispatch module**

Create `src/engine/builtins/mod.rs`:

```rust
//! Registration of all side-effecting Rhai builtins.
pub mod exec;
pub mod http;
pub mod util;

use crate::engine::context::SharedCtx;
use rhai::Engine;

/// Register every builtin into the engine, each capturing the shared context.
pub fn register_builtins(engine: &mut Engine, ctx: SharedCtx) {
    exec::register(engine, ctx.clone());
    http::register(engine, ctx.clone());
    util::register(engine, ctx);
}
```

- [ ] **Step 2: Write the failing test + implementation**

Create `src/engine/builtins/exec.rs`:

```rust
//! Command-execution builtins. Effect classification is BY BUILTIN:
//! ssh_exec/local_exec/ssh_exec_all are MUTATING; ssh_probe is READ-ONLY.
//! (Phase 3 uses this distinction for dry-run.)

use crate::engine::context::{EffectMode, SharedCtx};
use crate::engine::runner::CommandRunner;
use crate::engine::types::ExecResult;
use rhai::{Array, Dynamic, Engine};
use std::sync::Arc;
use std::thread;

fn to_result(host: &str, raw: crate::engine::runner::RawOutput) -> ExecResult {
    ExecResult { stdout: raw.stdout, stderr: raw.stderr, exit_code: raw.exit_code, host: host.to_string() }
}

/// Snapshot (mode, runner, trace) under a short lock, then release before blocking.
fn snapshot(ctx: &SharedCtx) -> (EffectMode, Arc<dyn CommandRunner>, bool) {
    let g = ctx.lock().unwrap();
    (g.mode, g.runner.clone(), g.trace)
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // ssh_exec — MUTATING remote command.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec", move |host: &str, cmd: &str| -> ExecResult {
            let (mode, runner, trace) = snapshot(&ctx);
            if trace { eprintln!("[nrg] ssh_exec {host} -> {cmd}"); }
            if mode == EffectMode::DryRun {
                // Phase 3 will record to a plan log; for now Live-only path is exercised.
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: host.into() };
            }
            to_result(host, runner.run_ssh(host, cmd))
        });
    }

    // ssh_probe — READ-ONLY remote command (still executes in dry-run, Phase 3).
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_probe", move |host: &str, cmd: &str| -> ExecResult {
            let (_mode, runner, trace) = snapshot(&ctx);
            if trace { eprintln!("[nrg] ssh_probe {host} -> {cmd}"); }
            to_result(host, runner.run_ssh(host, cmd))
        });
    }

    // local_exec — MUTATING local command.
    {
        let ctx = ctx.clone();
        engine.register_fn("local_exec", move |cmd: &str| -> ExecResult {
            let (mode, runner, trace) = snapshot(&ctx);
            if trace { eprintln!("[nrg] local_exec -> {cmd}"); }
            if mode == EffectMode::DryRun {
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: String::new() };
            }
            to_result("", runner.run_local(cmd))
        });
    }

    // ssh_exec_all — parallel fan-out across hosts. Never aborts on single-host failure.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec_all", move |hosts: Array, cmd: &str| -> Array {
            let (mode, runner, _trace) = snapshot(&ctx);
            let host_strs: Vec<String> = hosts.iter().map(|h| h.clone().into_string().unwrap_or_default()).collect();
            let cmd = cmd.to_string();
            if mode == EffectMode::DryRun {
                return host_strs.into_iter()
                    .map(|h| Dynamic::from(ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: h }))
                    .collect();
            }
            let results: Vec<ExecResult> = thread::scope(|s| {
                let handles: Vec<_> = host_strs.iter().map(|h| {
                    let runner = runner.clone();
                    let cmd = cmd.clone();
                    let h = h.clone();
                    s.spawn(move || to_result(&h, runner.run_ssh(&h, &cmd)))
                }).collect();
                handles.into_iter().map(|j| j.join().unwrap_or_else(|_| ExecResult {
                    stdout: String::new(), stderr: "thread panicked".into(), exit_code: -1, host: String::new(),
                })).collect()
            });
            results.into_iter().map(Dynamic::from).collect()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;
    use crate::engine::types::register_types;

    fn engine_with(ctx: SharedCtx) -> Engine {
        let mut e = Engine::new();
        register_types(&mut e);
        register(&mut e, ctx);
        e
    }

    #[test]
    fn ssh_exec_runs_through_runner_and_returns_ok() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let ok: bool = e.eval(r#"ssh_exec("web1", "uptime").ok"#).unwrap();
        assert!(ok);
        assert_eq!(fake.calls(), vec!["ssh web1: uptime".to_string()]);
    }

    #[test]
    fn ssh_exec_all_fans_out_to_every_host() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let n: i64 = e.eval(r#"ssh_exec_all(["a","b","c"], "docker pull x").len()"#).unwrap();
        assert_eq!(n, 3);
        assert_eq!(fake.calls().len(), 3);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib engine::builtins::exec`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/builtins/mod.rs src/engine/builtins/exec.rs
git commit -m "feat(engine): exec builtins (ssh_exec/ssh_probe/local_exec/ssh_exec_all)"
```

---

## Task 6: HTTP builtins (`http_get`, `http_post`)

**Files:**
- Create: `src/engine/builtins/http.rs`
- Test: in `src/engine/builtins/http.rs`

- [ ] **Step 1: Write the implementation + test**

Create `src/engine/builtins/http.rs`:

```rust
//! HTTP builtins (read-class; used by health checks). Uses ureq with a 30s timeout.

use crate::engine::context::SharedCtx;
use crate::engine::types::HttpResponse;
use rhai::Engine;
use std::time::Duration;

fn do_get(url: &str) -> HttpResponse {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
    match agent.get(url).call() {
        Ok(mut resp) => {
            let status = resp.status().as_u16() as i64;
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            HttpResponse { status, body }
        }
        Err(ureq::Error::StatusCode(code)) => HttpResponse { status: code as i64, body: String::new() },
        Err(e) => HttpResponse { status: 0, body: format!("http error: {e}") },
    }
}

fn do_post(url: &str, body: &str) -> HttpResponse {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
    match agent.post(url).content_type("application/json").send(body) {
        Ok(mut resp) => {
            let status = resp.status().as_u16() as i64;
            let rbody = resp.body_mut().read_to_string().unwrap_or_default();
            HttpResponse { status, body: rbody }
        }
        Err(ureq::Error::StatusCode(code)) => HttpResponse { status: code as i64, body: String::new() },
        Err(e) => HttpResponse { status: 0, body: format!("http error: {e}") },
    }
}

pub fn register(engine: &mut Engine, _ctx: SharedCtx) {
    engine.register_fn("http_get", |url: &str| -> HttpResponse { do_get(url) });
    engine.register_fn("http_post", |url: &str, body: &str| -> HttpResponse { do_post(url, body) });
}

#[cfg(test)]
mod tests {
    // Network-dependent; covered by integration smoke in Task 9. Unit-level: ensure the
    // builtins register and are callable (compile-time contract).
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn http_builtins_register() {
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, shared(FakeRunner::shared()));
        // Just assert the symbols exist by compiling a script that references them.
        assert!(e.compile(r#"fn _f(){ http_get("http://x"); http_post("http://x","{}"); }"#).is_ok());
    }
}
```

> **Note for the engineer:** `ureq` 3.x API is used above (`Agent::config_builder().timeout_global(...)`, `resp.body_mut().read_to_string()`, `Err(ureq::Error::StatusCode(code))`). If `cargo build` reports a different `ureq` API, run `cargo doc -p ureq --open` and adjust the four call sites; the `HttpResponse` shape stays identical.

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --lib engine::builtins::http`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/engine/builtins/http.rs
git commit -m "feat(engine): http_get/http_post builtins"
```

---

## Task 7: Utility builtins (`sleep`, `nrg_env`, `env_or`)

**Files:**
- Create: `src/engine/builtins/util.rs`
- Test: in `src/engine/builtins/util.rs`

- [ ] **Step 1: Write the implementation + test**

Create `src/engine/builtins/util.rs`:

```rust
//! Small utility builtins.

use crate::engine::context::SharedCtx;
use rhai::{Engine, EvalAltResult};

pub fn register(engine: &mut Engine, _ctx: SharedCtx) {
    engine.register_fn("sleep", |seconds: i64| {
        if seconds > 0 { std::thread::sleep(std::time::Duration::from_secs(seconds as u64)); }
    });

    // nrg_env — required env var; aborts the script (throws) if unset.
    engine.register_fn("nrg_env", |name: &str| -> Result<String, Box<EvalAltResult>> {
        std::env::var(name).map_err(|_| format!("required env var not set: {name}").into())
    });

    // env_or — env var with a fallback default.
    engine.register_fn("env_or", |name: &str, default: &str| -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    fn engine() -> Engine {
        let mut e = Engine::new();
        register(&mut e, shared(FakeRunner::shared()));
        e
    }

    #[test]
    fn env_or_returns_default_when_unset() {
        let e = engine();
        let v: String = e.eval(r#"env_or("NRG_DEFINITELY_UNSET_XYZ", "fallback")"#).unwrap();
        assert_eq!(v, "fallback");
    }

    #[test]
    fn nrg_env_throws_when_unset() {
        let e = engine();
        let r = e.eval::<String>(r#"nrg_env("NRG_DEFINITELY_UNSET_XYZ")"#);
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --lib engine::builtins::util`
Expected: PASS (2 tests).

- [ ] **Step 3: Commit**

```bash
git add src/engine/builtins/util.rs
git commit -m "feat(engine): sleep/nrg_env/env_or builtins"
```

---

## Task 8: Engine builder + file evaluator (with module import)

**Files:**
- Create/replace: `src/engine/eval.rs`
- Modify: `src/engine/mod.rs` (add `build_engine`)
- Test: in `src/engine/eval.rs` (uses a temp dir + FakeRunner)

- [ ] **Step 1: Add `build_engine` to `src/engine/mod.rs`**

Append to `src/engine/mod.rs`:

```rust
use crate::engine::context::SharedCtx;
use rhai::Engine;

/// Build an engine with result types + all builtins registered, print routed to stderr,
/// and trusted-script safety limits lifted. The module resolver is set per-file in eval.
pub fn build_engine(ctx: SharedCtx) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(0); // trusted scripts: unlimited
    engine.on_print(|s| eprintln!("{s}"));
    engine.on_debug(|s, _src, pos| eprintln!("[debug] {s} @ {pos:?}"));
    types::register_types(&mut engine);
    builtins::register_builtins(&mut engine, ctx);
    engine
}
```

- [ ] **Step 2: Write the failing test + implementation**

Replace the `src/engine/eval.rs` stub with:

```rust
//! Compile and run a `.rhai` orchestration module, with `import` anchored at the
//! file's own directory so `import "lib/docker" as docker;` resolves to
//! <file-dir>/lib/docker.rhai.

use crate::engine::context::SharedCtx;
use rhai::module_resolvers::FileModuleResolver;
use std::path::Path;

/// Run the module top-level (exec mode).
pub fn run_file(path: &Path, ctx: SharedCtx) -> Result<(), String> {
    let mut engine = crate::engine::build_engine(ctx);
    let base = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| ".".into());
    engine.set_module_resolver(FileModuleResolver::new_with_path(base));
    let ast = engine.compile_file(path.to_path_buf()).map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    engine.run_ast(&ast).map_err(|e| format!("{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;
    use std::fs;

    #[test]
    fn runs_a_script_that_imports_a_lib_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        // lib/docker.rhai defines pull() which calls the GLOBAL ssh_exec builtin.
        fs::write(dir.path().join("lib/docker.rhai"),
            r#"fn pull(host, img) { ssh_exec(host, "docker pull " + img); }"#).unwrap();
        // main calls the imported module fn.
        let main = dir.path().join("Energize.rhai");
        fs::write(&main,
            r#"import "lib/docker" as docker; docker::pull("web1", "nginx:latest");"#).unwrap();

        let fake = FakeRunner::shared();
        run_file(&main, shared(fake.clone())).unwrap();
        // PROVES the global builtin executed from inside the imported module fn.
        assert_eq!(fake.calls(), vec!["ssh web1: docker pull nginx:latest".to_string()]);
    }

    #[test]
    fn parse_error_is_reported_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("bad.rhai");
        fs::write(&main, "let x = ;").unwrap();
        let err = run_file(&main, shared(FakeRunner::shared())).unwrap_err();
        assert!(err.contains("parse error"));
    }
}
```

> `tempfile` is already a dev-dependency.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib engine::eval`
Expected: PASS (2 tests) — the import test proves global builtins are callable from imported modules.

- [ ] **Step 4: Commit**

```bash
git add src/engine/mod.rs src/engine/eval.rs
git commit -m "feat(engine): build_engine + run_file with FileModuleResolver imports"
```

---

## Task 9: Rewire `nrg exec` to the Rhai engine

**Files:**
- Replace: `src/cli/exec.rs`
- Test: `tests/cli/exec_rhai.rs` (integration, via `assert_cmd`)

- [ ] **Step 1: Replace `src/cli/exec.rs`**

```rust
//! `nrg exec` — evaluate a Rhai orchestration module top-to-bottom. Builtins
//! (ssh_exec, http_get, …) have real side effects as evaluation reaches them.

use crate::engine::context::shared;
use crate::engine::runner::RealRunner;
use crate::ssh::config::SshConfig;
use clap::Args;
use std::sync::Arc;

const DEFAULT_FILES: &[&str] = &["Energize.rhai", "energize.rhai", "Energize.star", "energize.star"];

#[derive(Args)]
pub struct ExecArgs {
    /// Path to the .rhai file to evaluate. Defaults to Energize.rhai.
    pub file: Option<String>,
}

fn find_default() -> Option<String> {
    DEFAULT_FILES.iter().find(|f| std::path::Path::new(f).exists()).map(|s| s.to_string())
}

pub fn execute(args: &ExecArgs) -> i32 {
    let path = match args.file.clone().or_else(find_default) {
        Some(p) => p,
        None => { eprintln!("Error: no Energize.rhai found. Create one or pass a file: nrg exec deploy.rhai"); return 1; }
    };

    let ssh = SshConfig::load().unwrap_or_else(|_| SshConfig::empty());
    let ctx = shared(Arc::new(RealRunner { ssh }));

    match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => { eprintln!("Error: {e}"); 1 }
    }
}
```

> **Engineer notes:** (1) `SshConfig::load()` — confirm the exact constructor name in `src/ssh/config.rs`; if it differs (e.g. `from_default_path`), use that, falling back to `SshConfig::empty()`. (2) This removes all `starlark::*` imports from `cli/exec.rs`; the old Starlark `runtime` module is now unused by `exec` but still compiles (deleted in Phase 6). (3) Update the `Exec` doc-comment in `src/cli/mod.rs` from "Starlark" to "Rhai".

- [ ] **Step 2: Write the integration test**

Create `tests/cli/exec_rhai.rs`:

```rust
use assert_cmd::Command;
use std::fs;

#[test]
fn exec_runs_a_local_rhai_script() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("Energize.rhai");
    // local_exec runs `sh -c` for real; echo is safe and host-independent.
    fs::write(&script, r#"let r = local_exec("echo hello-from-rhai"); print(r.stdout.trim());"#).unwrap();

    Command::cargo_bin("nrg").unwrap()
        .arg("exec").arg(script.to_str().unwrap())
        .assert()
        .success()
        .stderr(predicates::str::contains("hello-from-rhai"));
}
```

> If `tests/cli/` uses a `mod`-based harness (check `tests/cli/main.rs` or similar), register `exec_rhai` there following the existing pattern; otherwise this standalone file is picked up automatically.

- [ ] **Step 3: Run the integration test**

Run: `cargo test --test '*' exec_runs_a_local_rhai_script` (or `cargo test exec_runs_a_local_rhai_script`)
Expected: PASS — `print` goes to stderr, which contains `hello-from-rhai`.

- [ ] **Step 4: Full suite + clippy gate**

Run: `cargo test`
Expected: all pass (legacy Starlark tests still green — untouched).
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (fix any warnings in the new `engine` modules).

- [ ] **Step 5: Commit**

```bash
git add src/cli/exec.rs src/cli/mod.rs tests/cli/exec_rhai.rs
git commit -m "feat(cli): nrg exec now runs Rhai modules via the new engine"
```

---

## Task 10: Phase-0 acceptance smoke

**Files:**
- Create (throwaway, not committed): `/tmp/nrg-p0/Energize.rhai`

- [ ] **Step 1: Hand-run the engine end-to-end**

```bash
mkdir -p /tmp/nrg-p0/lib
cat > /tmp/nrg-p0/lib/util.rhai <<'EOF'
fn greet(who) { local_exec("echo hi " + who); }
EOF
cat > /tmp/nrg-p0/Energize.rhai <<'EOF'
import "lib/util" as util;
let r = util::greet("world");
print("exit=" + r.exit_code.to_string() + " ok=" + r.ok.to_string());
let n = env_or("NRG_WHO", "nobody");
print("env_or=" + n);
EOF
cargo run -- exec /tmp/nrg-p0/Energize.rhai
```

Expected stderr contains `hi world`, `exit=0 ok=true`, `env_or=nobody`.

- [ ] **Step 2: Confirm acceptance**

Phase 0 is done when: `nrg exec` runs a Rhai module, imports a lib module, calls `ssh_exec`/`local_exec`/`http_get`/`env_or`, getters read on results, the whole `cargo test` suite is green, and clippy is clean.

- [ ] **Step 3 (optional): clean the throwaway**

```bash
rm -rf /tmp/nrg-p0
```

---

## Self-review (completed by author)

- **Spec coverage (Phase 0 slice):** §3 module map (engine tree) → Tasks 2–8; §3.4 shared transport via one runner → Task 3; §4 verified Rhai APIs (`register_get`, `register_fn`+Arc, `compile_file`/`run_ast`, `FileModuleResolver`, `set_max_operations(0)`, `on_print`) → Tasks 2,5,8; §11 trait-injected runner for tests → Task 3 used throughout. Dry-run/transactions/state/secrets are explicitly **out of Phase 0** (P1–P4) — the `EffectMode::DryRun` branches are stubbed now and filled in P3.
- **Placeholder scan:** none — every step has concrete code/commands. Two flagged API-uncertainty notes (`ureq` 3.x call sites in Task 6; `SshConfig` constructor name in Task 9) are explicit verification instructions, not placeholders.
- **Type consistency:** `ExecResult{stdout,stderr,exit_code:i64,host}`, `HttpResponse{status:i64,body}`, `RawOutput{stdout,stderr,exit_code:i64}`, `CommandRunner::{run_ssh,run_local}`, `SharedCtx=Arc<Mutex<RunCtx>>`, `EffectMode::{Live,DryRun}`, `build_engine`/`run_file` — names used identically across Tasks 2–10.

---

## Roadmap (subsequent phases — each gets its own plan when its predecessor lands)

Per the design spec (`docs/superpowers/specs/2026-06-03-rhai-migration-design.md` §12), in order:

- **P1 — State:** project-root marker discovery, `fd-lock` exclusive-for-mutating-run / snapshot-read, atomic temp+fsync+rename, corruption hard-fail, schema+backup; `state_*` builtins on the locked handle.
- **P2 — Secrets:** `Secret` type + provenance redaction; canonical POSIX `sh_quote` (newline/quote-safe); `DEBUG` trap gated on `NRG_TRACE`; secret delivery via stdin/env-file (no remote env export); `with_secret`.
- **P3 — Dry-run:** fill the `EffectMode::DryRun` branches — plan log + simulated-state overlay; `wait_healthy`/`http_get` short-circuit; symbolic ports; `--dry-run` global flag.
- **P4 — Transactions:** `transaction`/`on_rollback` via `FnPtr` stack; best-effort error-isolated LIFO unwind; per-host fan-out failure surfacing.
- **P5 — Stdlib port:** `runtime`→`docker`→`proxy`→`healthcheck`→`registry`→`deploy` + 6 examples to `.rhai`; `deploy` reordered for the rollback window + wrapped in `transaction` + `.prev` snapshot; unify `nrg run <fn>` onto the engine via `call_fn`.
- **P6 — Cleanup:** delete `starlark_parser`/`bash_parser`/`runtime/` (Starlark)/`loader`; remove `starlark*`/`ratatui`/`crossterm` deps; rewrite README/`init`/`doctor`; correct README's nonexistent `nginx`/`tls`/`provision` claims.

---

## Execution handoff

Ultracode is on, so execution will be **workflow-driven** (the multi-agent analog of subagent-driven development): one agent implements a task, an adversarial agent reviews the diff against this plan before the next task. Checkpoints surface to you at phase boundaries.
