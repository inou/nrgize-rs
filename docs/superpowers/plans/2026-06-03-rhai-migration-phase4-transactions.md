# Rhai Migration — Phase 4: Transactions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `transaction(|| { ... })` + `on_rollback(|| { ... })` so a deploy that `throw`s
mid-way automatically unwinds the compensations registered so far — **LIFO, best-effort,
error-isolated** (one failing compensation doesn't abort the rest) — then re-raises. This is
the architectural gap from the original critique: a mid-fleet failure can revert the hosts it
already touched.

**Architecture:** `RunCtx` gains `txn: Arc<Mutex<TxnState { comps: Vec<FnPtr>, depth: usize }>>`.
`on_rollback(cb)` pushes the closure's `FnPtr` (live) or records a plan line (dry-run, so the
FnPtr — which may have real side effects — is never invoked). `transaction(body)` takes a
`NativeCallContext` (auto-injected first param) so it can invoke the body and the compensations
via `FnPtr::call_within_context` (no explicit `&AST` needed). It records `mark = comps.len()`
and `depth += 1` on entry; on success it discards its comps only if outermost (`depth == 0`,
keeping them for an enclosing txn otherwise); on `throw` it `split_off(mark)`s its comps and
runs them reversed, catch-and-logging each, then re-raises the original error.

**Tech Stack:** `rhai::{FnPtr, NativeCallContext}` (sync feature already on).

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/transaction.rs` | `TxnState`, `transaction`/`on_rollback` builtins + registration. |
| `src/engine/context.rs` | `RunCtx.txn` field. |
| `src/engine/mod.rs` | register the transaction builtins in `build_engine`. |

---

## Task 1: TxnState on RunCtx

**Files:** Modify `src/engine/context.rs`

- [ ] **Step 1:** Add to `context.rs`:

```rust
use rhai::FnPtr;

/// Active-transaction state: the compensation stack + nesting depth.
#[derive(Default)]
pub struct TxnState {
    pub comps: Vec<FnPtr>,
    pub depth: usize,
}
```

Add the field to `struct RunCtx` (after `plan`):

```rust
    /// Compensation stack for transaction()/on_rollback().
    pub txn: Arc<Mutex<TxnState>>,
```

Initialize in `RunCtx::build`:

```rust
            txn: Arc::new(Mutex::new(TxnState::default())),
```

- [ ] **Step 2:** `cargo build` → compiles (field unused until Task 2 — that's fine for one
intermediate step; Task 2 lands immediately).

---

## Task 2: transaction + on_rollback builtins

**Files:** Create `src/engine/transaction.rs`; Modify `src/engine/mod.rs`

- [ ] **Step 1: Create `src/engine/transaction.rs`**

```rust
//! Transaction / compensation-stack builtins. A `transaction(|| {...})` body that throws
//! unwinds the `on_rollback(|| {...})` closures registered so far — LIFO, best-effort,
//! error-isolated — then re-raises.

use crate::engine::context::{EffectMode, SharedCtx};
use rhai::{Engine, EvalAltResult, FnPtr, NativeCallContext};

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // on_rollback(cb) — register a compensation (live) or record it (dry-run, never invoked).
    {
        let ctx = ctx.clone();
        engine.register_fn("on_rollback", move |cb: FnPtr| {
            let mode = ctx.lock().unwrap().mode;
            if mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("rollback", None, "register compensation".into());
            } else {
                ctx.lock().unwrap().txn.lock().unwrap().comps.push(cb);
            }
        });
    }

    // transaction(body) — run body; on throw, unwind compensations registered during it.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "transaction",
            move |context: NativeCallContext, body: FnPtr| -> Result<(), Box<EvalAltResult>> {
                // Enter: remember our starting point and bump nesting depth.
                let mark = {
                    let g = ctx.lock().unwrap();
                    let mut t = g.txn.lock().unwrap();
                    t.depth += 1;
                    t.comps.len()
                };

                let result: Result<(), Box<EvalAltResult>> =
                    body.call_within_context::<()>(&context, ());

                match result {
                    Ok(()) => {
                        let g = ctx.lock().unwrap();
                        let mut t = g.txn.lock().unwrap();
                        t.depth -= 1;
                        // Outermost commit: drop our comps. Nested success: keep them so an
                        // enclosing transaction's failure still unwinds them (flatten).
                        if t.depth == 0 {
                            t.comps.truncate(mark);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        // Take just OUR comps (mark..end) and run them LIFO, best-effort.
                        let to_run = {
                            let g = ctx.lock().unwrap();
                            let mut t = g.txn.lock().unwrap();
                            t.depth -= 1;
                            t.comps.split_off(mark)
                        };
                        for comp in to_run.iter().rev() {
                            if let Err(ce) = comp.call_within_context::<()>(&context, ()) {
                                eprintln!("[nrg] rollback step failed (continuing): {ce}");
                            }
                        }
                        Err(e) // re-raise the original failure
                    }
                }
            },
        );
    }
}
```

- [ ] **Step 2: Register in `build_engine`** — in `src/engine/mod.rs`, after `secret::register`:

```rust
    transaction::register(&mut engine, ctx.clone());
```

and add `pub mod transaction;` to the module list, and change the final `secret::register(&mut engine, ctx);` to `secret::register(&mut engine, ctx.clone());` so `ctx` is still available.

- [ ] **Step 3: Unit tests** in `src/engine/transaction.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{shared, EffectMode};
    use crate::engine::runner::FakeRunner;
    use std::sync::{Arc, Mutex};

    /// Build an engine wired to a ctx, with a Rust-side `log(s)` builtin that appends to a
    /// shared Vec so tests can observe ordering of body + compensation execution.
    fn engine_with_log(ctx: SharedCtx) -> (Engine, Arc<Mutex<Vec<String>>>) {
        let mut e = Engine::new();
        register(&mut e, ctx);
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        e.register_fn("log", move |s: &str| l.lock().unwrap().push(s.to_string()));
        (e, log)
    }

    #[test]
    fn throw_unwinds_compensations_lifo() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        let script = r#"
            try {
                transaction(|| {
                    on_rollback(|| log("undo-1"));
                    log("do-1");
                    on_rollback(|| log("undo-2"));
                    log("do-2");
                    throw "boom";
                });
            } catch(e) { log("caught:" + e); }
        "#;
        e.run(script).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["do-1", "do-2", "undo-2", "undo-1", "caught:boom"] // LIFO unwind
        );
    }

    #[test]
    fn success_runs_no_compensations() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        e.run(r#"transaction(|| { on_rollback(|| log("undo")); log("do"); });"#).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["do"]); // no undo on success
    }

    #[test]
    fn throwing_compensation_does_not_abort_unwind() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        let script = r#"
            try {
                transaction(|| {
                    on_rollback(|| log("undo-1"));
                    on_rollback(|| { log("undo-2-start"); throw "comp-fail"; });
                    on_rollback(|| log("undo-3"));
                    throw "boom";
                });
            } catch(e) { log("caught"); }
        "#;
        e.run(script).unwrap();
        // undo-3 runs, undo-2 throws (logged+continues), undo-1 still runs.
        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["undo-3", "undo-2-start", "undo-1", "caught"]);
    }

    #[test]
    fn sequential_transactions_do_not_cross_unwind() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        let script = r#"
            transaction(|| { on_rollback(|| log("undo-A")); });   // commits, discards undo-A
            try {
                transaction(|| { on_rollback(|| log("undo-B")); throw "x"; });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["undo-B"]); // NOT undo-A
    }

    #[test]
    fn dry_run_records_rollback_and_does_not_invoke() {
        let ctx = shared(FakeRunner::shared());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let (e, log) = engine_with_log(ctx.clone());
        let script = r#"
            try {
                transaction(|| { on_rollback(|| log("undo")); throw "boom"; });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        assert!(log.lock().unwrap().is_empty(), "compensation must NOT run in dry-run");
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "rollback"));
    }
}
```

- [ ] **Step 4: Run** `cargo test --bin nrg engine::transaction` → PASS (5 tests). Then full
`cargo test` and `cargo clippy --all-targets 2>&1 | grep -E "src/engine|src/cli"` → empty.

- [ ] **Step 5: Commit**

```bash
git add src/engine/context.rs src/engine/transaction.rs src/engine/mod.rs
git commit -m "feat(txn): transaction()/on_rollback() compensation stack (LIFO best-effort unwind)"
```

---

## Task 3: Integration test + acceptance + review

- [ ] **Step 1: Integration test** — Create `tests/transaction.rs`:

```rust
use assert_cmd::Command;
use std::fs;

#[test]
fn failed_transaction_unwinds_via_local_exec() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let marker = dir.path().join("rolled-back");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
        transaction(|| {{
            on_rollback(|| {{ local_exec("touch {m}"); }});
            local_exec("true");          // a real step
            throw "deploy failed on host 3";
        }});
        "#,
            m = marker.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure() // the throw re-raises after unwinding
        .stderr(predicates::str::contains("deploy failed on host 3"));

    // The compensation ran: the rollback marker file exists.
    assert!(marker.exists(), "rollback compensation should have executed");
}
```

- [ ] **Step 2: Run** `cargo test --test transaction` → PASS. Then full `cargo test`.

- [ ] **Step 3: Commit**

```bash
git add tests/transaction.rs
git commit -m "test(txn): integration — failed transaction runs its compensations"
```

- [ ] **Step 4: Adversarial review** — lenses: unwind correctness (LIFO, best-effort,
re-raise; nested/sequential semantics; lock release before invoking comps to avoid deadlock if
a comp touches state/txn), FnPtr/NativeCallContext correctness (does `call_within_context`
behave; closure capture of shared values; Send/Sync under the `sync` feature), dry-run
record-not-invoke, and P5 forward-compat (does `deploy()` get what it needs to keep the old
container alive until commit — the §7 reorder). Fold fix-now.

---

## Phase 4 review outcome (adversarial workflow, 2026-06-03)

3-lens review (unwind, FnPtr/NativeCallContext, deadlock/forward) + verification, all probes
reverted (tree clean). Verified correct: 4 nesting scenarios (sequential, inner-success/outer-
fail, inner-fail/outer-fail, inner-fail/outer-catch) each run every comp exactly once LIFO; the
re-raised error is the original body error; `FnPtr::call_within_context` + `NativeCallContext`-
first-param + `FnPtr: Send+Sync` under `sync` all correct against rhai 1.25.1; **no deadlock /
no lock-ordering inversion** (ctx/txn locks released before invoking comps — empirically
attacked).

**Fixed in P4 (HIGH):** a compensation that calls `on_rollback` *during* unwind was silently
lost (the `split_off` snapshot missed the live push) and leaked residue. Now drained via a
**pop-loop** (regression test `reentrant_on_rollback_during_unwind_is_drained` + a
`compensation_calling_a_ctx_locking_builtin_does_not_deadlock` test).

**Deferred:**
- **MEDIUM — panic-safety of `depth`:** a Rust *panic* (not `throw`) in a body/comp bypasses
  both match arms, leaking `txn.depth` and poisoning the ctx mutex. Moot today (a panic on the
  eval thread aborts the process), but if a panic were ever caught mid-run, later transactions
  would mis-scope. Fix later with an RAII depth-guard, or declare panics fatal explicitly.
- **MEDIUM — plan visibility:** `transaction()` records no begin/commit markers, so dry-run
  plans don't show transaction boundaries. Add `txn` begin/commit plan lines in a polish pass.
- **⚠️ P5 (HIGH) — `deploy()` reorder (spec §7.3/§7.6):** the API *can* express the safe
  ordering (register the proxy-switch inverse BEFORE switching; defer old-container destruction
  to AFTER the `transaction` block, since `transaction()` re-raises on failure). But the legacy
  `deploy_to_host` stops/renames/removes the old container *inside* the deploy window — P5 MUST
  reorder it. The flattened fleet deploy (§7.6) has no per-host post-commit point; P5 must
  choose the cleanup strategy (per-host commit vs fleet manifest). An `on_commit` hook is NOT
  strictly required for a single host.

## Self-review (author)

- **Spec §7 coverage:** `transaction`/`on_rollback` → T2; best-effort error-isolated LIFO
  unwind → T2 + tests; register-before-effect is the script-author contract (deploy() in P5
  will honor it); dry-run records-not-invokes → T2 + test; nested = flatten (depth/mark) → T2 +
  sequential test. **Deferred to P5:** the `deploy_to_host` reorder (keep old container alive
  until post-commit) — that's stdlib logic written against these primitives.
- **Placeholders:** none. **Types:** `TxnState{comps,depth}`, `RunCtx.txn`, `transaction`,
  `on_rollback`, `call_within_context`, `split_off(mark)` — consistent T1–T3.
