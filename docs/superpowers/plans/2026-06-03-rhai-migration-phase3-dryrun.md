# Rhai Migration — Phase 3: Dry-run — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `nrg exec --dry-run` evaluates the script without side effects, classified **by
builtin** (not by parsing commands): mutating builtins record a `PlannedAction` and return a
synthetic `ok=true` instead of executing; reads stay consistent via an in-memory overlay
(state) and short-circuits (http health → 200, `sleep` → skip); a plan log prints at the end.
Dry-run takes **no exclusive lock** and **never writes** disk state.

**Architecture:** A `Snapshot` (taken under a short `RunCtx` lock, then released) carries
`mode`, `runner`, `state`, `secrets`, `plan`, `trace` to every builtin. In `DryRun`, mutating
builtins push to `RunCtx.plan` (a `Vec<PlannedAction>`); `state_set/del` mutate an **overlay
store** (a `StateStore` with `root = None`, seeded from disk, so flush is a no-op and
`state_get` stays consistent); `http_get/http_post` return synthetic 200; `sleep` returns
immediately. `cli/exec` parses `--dry-run`, skips the flock, loads the overlay, sets the mode,
runs, and renders the plan.

**Tech Stack:** existing engine; no new deps.

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/plan.rs` | `PlannedAction` + `render_plan()` (summary + per-line, redacted upstream). |
| `src/engine/context.rs` | `RunCtx.plan` + `Snapshot` struct + `RunCtx::snapshot()`. |
| `src/engine/state.rs` | `StateStore::load_overlay()` (seeded, no-flush). |
| `src/engine/builtins/exec.rs` | record-not-execute in `DryRun` (ssh_exec/local_exec/ssh_exec_all). |
| `src/engine/builtins/state.rs` | record `state_set/del` in `DryRun`; reads from overlay. |
| `src/engine/builtins/http.rs`, `util.rs` | ctx-aware: dry-run short-circuit (http→200, sleep→skip). |
| `src/cli/exec.rs` | `--dry-run` flag → skip lock, overlay store, set mode, render plan. |

---

## Task 1: PlannedAction + plan log + Snapshot

**Files:** Create `src/engine/plan.rs`; Modify `src/engine/mod.rs`, `src/engine/context.rs`

- [ ] **Step 1: Create `src/engine/plan.rs`**

```rust
//! The dry-run plan log: a record of the side effects a run WOULD perform.

/// One side-effecting action that dry-run recorded instead of executing.
#[derive(Debug, Clone)]
pub struct PlannedAction {
    /// Short kind tag: "local", "ssh", "ssh-all", "state", "check".
    pub kind: String,
    /// Host(s) the action targets, if any.
    pub host: Option<String>,
    /// Human-readable detail (command / key=value), ALREADY redacted by the caller.
    pub detail: String,
}

/// Render the plan as a human-readable block (caller has already redacted details).
pub fn render_plan(actions: &[PlannedAction]) -> String {
    use std::collections::BTreeSet;
    let mut out = String::from("\nPLAN (dry run — no changes made):\n");
    if actions.is_empty() {
        out.push_str("  (no side effects)\n");
    }
    for a in actions {
        let host = a.host.as_deref().unwrap_or("-");
        out.push_str(&format!("  {:<7} {:<22} {}\n", a.kind, host, a.detail));
    }
    let hosts: BTreeSet<&str> = actions.iter().filter_map(|a| a.host.as_deref()).collect();
    out.push_str(&format!(
        "{} action(s), {} host(s). 0 executed.\n",
        actions.len(),
        hosts.len()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_summarizes_actions_and_hosts() {
        let actions = vec![
            PlannedAction { kind: "local".into(), host: None, detail: "docker build".into() },
            PlannedAction { kind: "ssh".into(), host: Some("a".into()), detail: "docker pull".into() },
            PlannedAction { kind: "ssh".into(), host: Some("b".into()), detail: "docker pull".into() },
        ];
        let r = render_plan(&actions);
        assert!(r.contains("docker build"));
        assert!(r.contains("3 action(s), 2 host(s). 0 executed."));
    }

    #[test]
    fn render_empty_plan() {
        assert!(render_plan(&[]).contains("0 action(s), 0 host(s). 0 executed."));
    }
}
```

- [ ] **Step 2:** Add `pub mod plan;` to `src/engine/mod.rs`.

- [ ] **Step 3: Add `plan` + `Snapshot` to `src/engine/context.rs`**

Add imports:

```rust
use crate::engine::plan::PlannedAction;
```

Add to `struct RunCtx` (after `secrets`):

```rust
    /// Recorded side effects, populated in DryRun mode.
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
```

Initialize in `RunCtx::build`:

```rust
            plan: Arc::new(Mutex::new(Vec::new())),
```

Add the `Snapshot` struct + method (after the `RunCtx` impl, before `SharedCtx`):

```rust
/// A consistent snapshot of the shared handles, taken under a short lock and then released so
/// builtins never hold the `RunCtx` lock across a blocking command.
pub struct Snapshot {
    pub mode: EffectMode,
    pub runner: Arc<dyn CommandRunner>,
    pub state: Arc<Mutex<StateStore>>,
    pub secrets: Arc<Mutex<HashSet<String>>>,
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
    pub trace: bool,
}

impl RunCtx {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            mode: self.mode,
            runner: self.runner.clone(),
            state: self.state.clone(),
            secrets: self.secrets.clone(),
            plan: self.plan.clone(),
            trace: self.trace,
        }
    }

    /// Record a planned action (dry-run).
    pub fn record(&self, kind: &str, host: Option<&str>, detail: String) {
        self.plan.lock().unwrap().push(PlannedAction {
            kind: kind.to_string(),
            host: host.map(|h| h.to_string()),
            detail,
        });
    }
}
```

> Note: the two `impl RunCtx` blocks can be merged; either is fine.

- [ ] **Step 4: Run** `cargo test --bin nrg engine::plan engine::context` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/engine/plan.rs src/engine/mod.rs src/engine/context.rs
git commit -m "feat(dry-run): PlannedAction + plan log + RunCtx::snapshot()"
```

---

## Task 2: Exec builtins record-not-execute in dry-run

**Files:** Modify `src/engine/builtins/exec.rs`

- [ ] **Step 1: Replace the snapshot helper + use the struct**

Replace the `snapshot`/`traced` helpers and update each builtin to take a `Snapshot`. The new
top of the registration logic:

```rust
use crate::engine::context::{EffectMode, RunCtx, Snapshot};
// (remove the old `fn snapshot` returning a tuple)

/// Redact a command for display against the registered secret values.
fn traced(cmd: &str, snap: &Snapshot) -> String {
    crate::engine::secret::redact(cmd, &snap.secrets.lock().unwrap())
}
```

`ssh_exec` becomes:

```rust
        engine.register_fn("ssh_exec", move |host: &str, cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_exec {host} -> {}", traced(cmd, &snap));
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("ssh", Some(host), traced(cmd, &snap));
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: host.into() };
            }
            to_result(host, snap.runner.run_ssh(host, cmd))
        });
```

`local_exec`:

```rust
        engine.register_fn("local_exec", move |cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] local_exec -> {}", traced(cmd, &snap));
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("local", None, traced(cmd, &snap));
                return ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 0, host: String::new() };
            }
            to_result("", snap.runner.run_local(cmd))
        });
```

`ssh_probe` (READ — always executes, just update the snapshot call):

```rust
        engine.register_fn("ssh_probe", move |host: &str, cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_probe {host} -> {}", traced(cmd, &snap));
            }
            to_result(host, snap.runner.run_ssh(host, cmd))
        });
```

`ssh_exec_all` (after host validation; the dry-run branch records one action per host):

```rust
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_exec_all -> {}", traced(cmd, &snap));
            }
            // ... host validation unchanged ...
            let cmd = cmd.to_string();
            if snap.mode == EffectMode::DryRun {
                let detail = crate::engine::secret::redact(&cmd, &snap.secrets.lock().unwrap());
                for h in &host_strs {
                    ctx.lock().unwrap().record("ssh-all", Some(h), detail.clone());
                }
                return Ok(host_strs.into_iter().map(|h| Dynamic::from(ExecResult {
                    stdout: String::new(), stderr: String::new(), exit_code: 0, host: h,
                })).collect());
            }
            let runner = snap.runner;
            // ... thread::scope fan-out uses `runner` (was `snap.runner`) ...
```

> Replace remaining `runner`/`mode` references with `snap.runner` etc. The `RunCtx` import is
> only needed if you reference it directly; `Snapshot`/`EffectMode` are the used ones.

- [ ] **Step 2: Update the redaction test** (it called `super::traced(cmd, &secrets)` — now `traced` takes a `Snapshot`):

```rust
    #[test]
    fn trace_redacts_registered_secret() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().register_secret("supersecretvalue");
        let snap = ctx.lock().unwrap().snapshot();
        assert_eq!(super::traced("docker login -p supersecretvalue", &snap), "docker login -p ***");
    }
```

- [ ] **Step 3: Add a dry-run recording test**

```rust
    #[test]
    fn dry_run_records_instead_of_executing() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let mut e = Engine::new();
        register_types(&mut e);
        register(&mut e, ctx.clone());
        let ok: bool = e.eval(r#"ssh_exec("web1", "rm -rf /data").ok"#).unwrap();
        assert!(ok); // synthetic ok
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, "ssh");
        assert_eq!(plan[0].detail, "rm -rf /data");
    }
```

- [ ] **Step 4: Run** `cargo test --bin nrg engine::builtins::exec` → PASS. Then `cargo clippy --all-targets 2>&1 | grep src/engine` → empty.

- [ ] **Step 5: Commit**

```bash
git add src/engine/builtins/exec.rs
git commit -m "feat(dry-run): exec builtins record planned actions instead of executing"
```

---

## Task 3: State overlay + dry-run state recording

**Files:** Modify `src/engine/state.rs`, `src/engine/builtins/state.rs`

- [ ] **Step 1: Add `load_overlay` to `src/engine/state.rs`** (in `impl StateStore`, after `load`):

```rust
    /// Load the on-disk data into an in-memory OVERLAY (root = None ⇒ flush is a no-op). Used
    /// by dry-run so `state_set`/`state_del` stay consistent for subsequent `state_get`s
    /// without ever touching disk.
    #[allow(dead_code)] // wired by cli/exec dry-run path
    pub fn load_overlay(root: &Path) -> Result<Self, String> {
        let loaded = Self::load(root)?;
        Ok(StateStore {
            root: None,
            data: loaded.data,
        })
    }
```

- [ ] **Step 2: Add a test:**

```rust
    #[test]
    fn overlay_seeds_from_disk_but_never_writes() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = StateStore::load(tmp.path()).unwrap();
            s.set("seeded", "yes").unwrap();
        }
        let mut overlay = StateStore::load_overlay(tmp.path()).unwrap();
        assert_eq!(overlay.get("seeded"), Some("yes".into())); // seeded from disk
        overlay.set("ghost", "1").unwrap(); // mutates memory only
        assert_eq!(overlay.get("ghost"), Some("1".into()));
        // Disk is untouched: a fresh load doesn't see `ghost`.
        let disk = StateStore::load(tmp.path()).unwrap();
        assert_eq!(disk.get("ghost"), None);
    }
```

- [ ] **Step 3: Record `state_set`/`state_del` in dry-run** — in `src/engine/builtins/state.rs`,
update `state_set` and `state_del` to also record when in dry-run. `state_set`:

```rust
        engine.register_fn(
            "state_set",
            move |key: &str, value: &str| -> Result<(), Box<EvalAltResult>> {
                let (mode, store) = {
                    let g = ctx.lock().unwrap();
                    (g.mode, g.state.clone())
                };
                if mode == crate::engine::context::EffectMode::DryRun {
                    ctx.lock().unwrap().record("state", None, format!("{key} = {value}"));
                }
                store.lock().unwrap().set(key, value).map_err(|e| e.into())
            },
        );
```

Apply the same `mode`-aware `record("state", None, format!("del {key}"))` to `state_del`.
(`state_get`/`state_all`/`has_state` are reads — unchanged; they read the overlay in dry-run.)

- [ ] **Step 4: Run** `cargo test --bin nrg engine::state engine::builtins::state` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/engine/state.rs src/engine/builtins/state.rs
git commit -m "feat(dry-run): state overlay (seeded, no-flush) + record state writes"
```

---

## Task 4: http/sleep ctx-aware dry-run short-circuit

**Files:** Modify `src/engine/builtins/http.rs`, `src/engine/builtins/util.rs`

- [ ] **Step 1: Make http builtins ctx-aware** — replace `src/engine/builtins/http.rs`
`register`:

```rust
pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    {
        let ctx = ctx.clone();
        engine.register_fn("http_get", move |url: &str| -> HttpResponse {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.mode == crate::engine::context::EffectMode::DryRun {
                ctx.lock().unwrap().record("check", None, format!("[assumed healthy] GET {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_get(url)
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("http_post", move |url: &str, body: &str| -> HttpResponse {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.mode == crate::engine::context::EffectMode::DryRun {
                ctx.lock().unwrap().record("check", None, format!("[assumed ok] POST {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_post(url, body)
        });
    }
}
```

> Add `use crate::engine::context::SharedCtx;` if not already imported. `do_get`/`do_post`
> unchanged.

- [ ] **Step 2: Make sleep ctx-aware** — in `src/engine/builtins/util.rs`, capture ctx and skip
in dry-run:

```rust
pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    {
        let ctx = ctx.clone();
        engine.register_fn("sleep", move |seconds: i64| {
            let mode = ctx.lock().unwrap().mode;
            if mode == crate::engine::context::EffectMode::DryRun {
                return; // don't actually sleep in dry-run
            }
            if seconds > 0 {
                std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
            }
        });
    }
    // nrg_env / env_or unchanged (pure reads):
    engine.register_fn("nrg_env", |name: &str| -> Result<String, Box<EvalAltResult>> {
        std::env::var(name).map_err(|_| format!("required env var not set: {name}").into())
    });
    engine.register_fn("env_or", |name: &str, default: &str| -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    });
}
```

- [ ] **Step 3: Add a dry-run http test** to `http.rs` tests:

```rust
    #[test]
    fn http_get_short_circuits_in_dry_run() {
        use crate::engine::context::{shared, EffectMode};
        use crate::engine::runner::FakeRunner;
        let ctx = shared(FakeRunner::shared());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        // Unreachable URL would error in Live mode; dry-run returns synthetic healthy 200.
        let ok: bool = e.eval(r#"http_get("http://127.0.0.1:1/never").ok"#).unwrap();
        assert!(ok);
    }
```

- [ ] **Step 4: Run** `cargo test --bin nrg engine::builtins` → PASS. `cargo clippy --all-targets 2>&1 | grep src/engine` → empty.

- [ ] **Step 5: Commit**

```bash
git add src/engine/builtins/http.rs src/engine/builtins/util.rs
git commit -m "feat(dry-run): http_get/post + sleep are ctx-aware (short-circuit in dry-run)"
```

---

## Task 5: `--dry-run` flag + cli wiring + plan output

**Files:** Modify `src/cli/exec.rs`

- [ ] **Step 1: Add the flag + wire it**

In `ExecArgs`:

```rust
    /// Show the plan of side effects without executing anything (no lock, no state writes).
    #[arg(long)]
    pub dry_run: bool,
```

In `execute`, change the lock + store + mode + plan handling. After resolving `path` and
`root`:

```rust
    use crate::engine::state;

    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => { eprintln!("Error: {e}"); return 1; }
    };

    // Dry-run takes NO lock and writes NO state.
    let (store, _guard, lock_holder_keepalive);
    if args.dry_run {
        store = match state::StateStore::load_overlay(&root) {
            Ok(s) => s,
            Err(e) => { eprintln!("Error: {e}"); return 1; }
        };
        _guard = None;
        lock_holder_keepalive = None;
    } else {
        // (existing lock acquisition: key/reentrant/open_lock/try_write/write …)
        // set lock_holder_keepalive = Some(lock_holder); _guard = Some(guard);
        // store = StateStore::load(&root)?  (existing)
        // — keep the existing block, just assign into these bindings.
        unreachable!("replace with existing lock+load block")
    }

    let ssh = SshConfig::load_default();
    let ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store);
    if args.dry_run {
        ctx.lock().unwrap().mode = crate::engine::context::EffectMode::DryRun;
    }
    let plan = ctx.lock().unwrap().plan.clone();

    let code = match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => { eprintln!("Error: {e}"); 1 }
    };
    if args.dry_run {
        print!("{}", crate::engine::plan::render_plan(&plan.lock().unwrap()));
    }
    code
```

> **Engineer note:** the `let (store, _guard, lock_holder_keepalive);` deferred-init pattern is
> finicky with the borrow of `lock_holder`. Simpler: keep two code paths. In the non-dry-run
> arm, inline the EXISTING lock-acquisition block (root discovery already done) and `load`,
> then build `ctx`. In the dry-run arm, `load_overlay` + no lock. Factor the shared tail
> (build ctx, run, maybe print plan) after the `if`. Whichever compiles cleanly — the contract
> is: **dry-run ⇒ no `open_lock`, no `StateStore::load` (use `load_overlay`), mode=DryRun,
> render plan at the end.**

- [ ] **Step 2: Integration test** — Create `tests/dry_run.rs`:

```rust
use assert_cmd::Command;
use std::fs;

#[test]
fn dry_run_records_plan_and_makes_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        local_exec("echo build > /tmp/nrg-dryrun-should-not-exist");
        state_set("version", "v9");
        let r = http_get("http://127.0.0.1:1/health");  // unreachable, but dry-run => ok
        if !r.ok { throw "health failed" }
        "#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stdout(predicates::str::contains("PLAN (dry run"))
        .stdout(predicates::str::contains("0 executed."));

    // No state.json was written (dry-run uses the overlay):
    assert!(!dir.path().join(".energize/state.json").exists());
    // The local_exec side effect did NOT happen:
    assert!(!std::path::Path::new("/tmp/nrg-dryrun-should-not-exist").exists());
}
```

- [ ] **Step 3: Run** `cargo test --test dry_run` → PASS. Then full `cargo test`. Then
`cargo clippy --all-targets 2>&1 | grep -E "src/engine|src/cli/exec"` → empty.

- [ ] **Step 4: Commit**

```bash
git add src/cli/exec.rs tests/dry_run.rs
git commit -m "feat(cli): nrg exec --dry-run (no lock, overlay state, plan output)"
```

---

## Task 6: Acceptance + adversarial review

- [ ] **Step 1: Hand-run** a dry-run vs live to confirm the plan log + zero side effects, and
that live still works:

```bash
mkdir -p /tmp/nrg-p3/.energize && cd /tmp/nrg-p3
cat > Energize.rhai <<'EOF'
local_exec("echo hi");
state_set("v", "1");
print("state v = " + state_get("v"));   // overlay keeps it consistent
EOF
echo "--- DRY RUN ---"; cargo run -q --manifest-path /Users/inou/dev/nrgize-rs/Cargo.toml -- exec --dry-run Energize.rhai
echo "state.json exists? (should be NO):"; ls .energize/state.json 2>/dev/null || echo "  none — good"
echo "--- LIVE ---"; cargo run -q --manifest-path /Users/inou/dev/nrgize-rs/Cargo.toml -- exec Energize.rhai
cat .energize/state.json
rm -rf /tmp/nrg-p3
```

Expected (dry-run): plan lists `local echo hi` + `state v = 1`, `state_get` returns `1` from
the overlay, NO `state.json`. Live: writes `state.json` with `v:1`.

- [ ] **Step 2: Adversarial review** — lenses: dry-run divergence (does any mutating builtin
still execute? does the overlay keep `state_get` consistent after stubbed `state_set`? does
`ssh_probe` correctly still run — and is that the right call?), lock/overlay correctness (truly
no lock + no disk write in dry-run), plan-log completeness/redaction, forward-compat with P4
(transactions in dry-run) and P5 (stdlib container reads need overlay-awareness). Fold fix-now.

---

## Phase 3 review outcome (adversarial workflow, 2026-06-03)

3-lens review (divergence, lock/plan, forward-compat) + verification. Interception verified
complete for current builtins; overlay confirmed consistent (single `RunCtx.state` Arc, no disk
writes); dry-run takes no lock / sets no `NRG_STATE_LOCK` / uses `load_overlay`; stdout(plan) /
stderr(prints+trace) split clean; `record()` is main-thread-only (no fan-out race).

**Fixed in P3 (HIGH):** `state_set`/`http` recorded **raw** values into the plan, which prints
to **stdout** (bypassing the stderr `on_print` redaction) — `state_set("x", reveal(secret))`
leaked plaintext to the plan. Now `RunCtx::record` redacts **every** detail at one boundary
(regression test added).

**⚠️ Carry into later phases:**
- **P4 (transactions):** `transaction`/`on_rollback` compensations must be registered in both
  modes, but in **DryRun recorded, not invoked** (a compensation `FnPtr` body may call the
  unguarded `ssh_probe` / real side effects). Add a `rollback` plan-kind.
- **P5 (stdlib) — the big one:** container-existence reads (`docker_container_running`,
  `_pick_port` nc probe, `wait_healthy`) will be built on `ssh_probe`, which has **no DryRun
  branch** and hits the live host — so create/skip branches diverge in dry-run (the §6 concern).
  The state overlay only models `state_*` keys, **not** container/host existence. **Before P5**:
  add a richer `SimState` (container overlay, seeded from one real `docker ps`) or mode-aware
  container-read builtins, and forbid stdlib reads from calling raw `ssh_probe` in dry-run.
  Standardize plan `kind` tags as constants and add docker/proxy distinctions.

**Deferred (low/documented):** synthetic `ssh_exec` stdout is empty, so stdout-parsing reads
diverge in dry-run (fundamental dry-run limitation; the P5 overlay/wrappers address it);
`http_get` returns an empty body (status-only short-circuit per spec); `upload`/`write_remote`
not yet ported (wire a DryRun branch when added).

## Self-review (author)

- **Spec §6 coverage:** classify by builtin (ssh_exec mutating / ssh_probe read) → T2; mutating
  record + synthetic ok → T2/T3; overlay so reads stay consistent → T3; http health short-circuit
  + sleep skip → T4; plan log + "N actions, 0 executed" → T1/T5; dry-run no lock / no state write
  → T5. **Deferred to P5:** symbolic `<auto>` ports and container-state reads
  (`docker_container_running`/`_pick_port`) — those builtins don't exist until the stdlib lands;
  P5's stdlib reads must consult the overlay / be mode-aware (recorded for the P5 plan).
- **Placeholders:** none except the explicitly-flagged "inline the existing lock block" note in
  T5 (a refactor instruction, not missing code). **Types:** `PlannedAction`, `render_plan`,
  `RunCtx.plan`, `Snapshot`, `snapshot()`, `record()`, `load_overlay`, `EffectMode::DryRun` —
  consistent T1–T6.
