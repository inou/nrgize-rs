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
                ctx.lock()
                    .unwrap()
                    .record("rollback", None, "register compensation".into());
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
                        // Take just OUR comps (mark..end), then release the lock BEFORE running
                        // them (a compensation may touch state/txn). Run LIFO, best-effort.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{shared, EffectMode};
    use crate::engine::runner::FakeRunner;
    use std::sync::{Arc, Mutex};

    /// Engine wired to `ctx`, plus a `log(s)` builtin appending to a shared Vec so tests can
    /// observe the ordering of body + compensation execution.
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
            vec!["do-1", "do-2", "undo-2", "undo-1", "caught:boom"]
        );
    }

    #[test]
    fn success_runs_no_compensations() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        e.run(r#"transaction(|| { on_rollback(|| log("undo")); log("do"); });"#)
            .unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["do"]);
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
        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["undo-3", "undo-2-start", "undo-1", "caught"]);
    }

    #[test]
    fn sequential_transactions_do_not_cross_unwind() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx);
        let script = r#"
            transaction(|| { on_rollback(|| log("undo-A")); });
            try {
                transaction(|| { on_rollback(|| log("undo-B")); throw "x"; });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["undo-B"]);
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
        assert!(
            log.lock().unwrap().is_empty(),
            "compensation must NOT run in dry-run"
        );
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "rollback"));
    }
}
