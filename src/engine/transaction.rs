//! Transaction / compensation-stack builtins. A `transaction(|| {...})` body that throws
//! unwinds the `on_rollback(|| {...})` closures registered so far — LIFO, best-effort,
//! error-isolated — then re-raises.

use crate::engine::context::{EffectMode, SharedCtx};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, NativeCallContext};

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // on_rollback(cb) — register a compensation (live) or record it (dry-run, never invoked).
    {
        let ctx = ctx.clone();
        engine.register_fn("on_rollback", move |cb: FnPtr| {
            if ctx.mode == EffectMode::DryRun {
                ctx.record("rollback", None, "register compensation".into());
            } else {
                ctx.txn.lock().unwrap().comps.push(cb);
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
                    let mut t = ctx.txn.lock().unwrap();
                    t.depth += 1;
                    t.comps.len()
                };

                // Call the body as a Dynamic and DISCARD the value. A natural Rhai body whose last
                // expression is value-returning (e.g. an ExecResult from ssh_exec with no trailing
                // `;`) must NOT be treated as a failure: a `::<()>` cast would raise
                // ErrorMismatchOutputType, spuriously firing every compensation on a SUCCESSFUL
                // body (issue #5). `::<Dynamic>` accepts any return.
                let result: Result<Dynamic, Box<EvalAltResult>> =
                    body.call_within_context::<Dynamic>(&context, ());

                match result {
                    Ok(_) => {
                        let mut t = ctx.txn.lock().unwrap();
                        t.depth -= 1;
                        // Outermost commit: drop our comps. Nested success: keep them so an
                        // enclosing transaction's failure still unwinds them (flatten).
                        if t.depth == 0 {
                            t.comps.truncate(mark);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        ctx.txn.lock().unwrap().depth -= 1;
                        // Drain comps above `mark` LIFO, popping ONE under a short lock each
                        // iteration and releasing BEFORE invoking it. Popping (vs a split_off
                        // snapshot) means a compensation that itself calls on_rollback during
                        // unwind pushes onto the live stack and is picked up by the next pop,
                        // instead of being silently lost / leaked. The compensation is likewise
                        // called as Dynamic so a value-returning comp isn't mis-logged as failed.
                        loop {
                            let comp = {
                                let mut t = ctx.txn.lock().unwrap();
                                if t.comps.len() > mark {
                                    t.comps.pop()
                                } else {
                                    None
                                }
                            };
                            match comp {
                                Some(c) => {
                                    if let Err(ce) = c.call_within_context::<Dynamic>(&context, ()) {
                                        eprintln!("[nrg] rollback step failed (continuing): {ce}");
                                    }
                                }
                                None => break,
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
    use crate::engine::context::shared;
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
    fn reentrant_on_rollback_during_unwind_is_drained() {
        let ctx = shared(FakeRunner::shared());
        let (e, log) = engine_with_log(ctx.clone());
        // A compensation that registers ANOTHER compensation while unwinding: the pop-loop
        // must drain the newly-registered one too (and leave no residue on the stack).
        let script = r#"
            try {
                transaction(|| {
                    on_rollback(|| { log("undo-1"); on_rollback(|| log("undo-1b")); });
                    throw "boom";
                });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["undo-1", "undo-1b"]);
        assert!(
            ctx.txn.lock().unwrap().comps.is_empty(),
            "no compensation residue should leak onto the stack"
        );
    }

    #[test]
    fn value_returning_body_does_not_spuriously_roll_back() {
        // Regression (#5): a body whose last expression is NOT unit (e.g. a builtin returning a
        // value, no trailing `;`) must COMMIT, not be misread as a failure that fires every comp.
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        e.register_fn("log", move |s: &str| l.lock().unwrap().push(s.to_string()));
        // `mk()` returns a value; the body's final expression is value-returning with no `;`.
        e.register_fn("mk", || 42_i64);
        let script = r#"
            transaction(|| {
                on_rollback(|| log("undo"));
                mk()      // value-returning final expression, no trailing ';'
            });
        "#;
        e.run(script).unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "a successful value-returning body must NOT fire compensations"
        );
        assert!(ctx.txn.lock().unwrap().comps.is_empty());
    }

    #[test]
    fn compensation_calling_a_ctx_locking_builtin_does_not_deadlock() {
        // Locks in the no-deadlock property: a compensation calls local_exec (which locks ctx
        // via snapshot) during unwind, while transaction() must NOT be holding ctx/txn.
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        crate::engine::builtins::exec::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        let script = r#"
            try {
                transaction(|| {
                    on_rollback(|| { local_exec("echo rollback"); });
                    throw "boom";
                });
            } catch(e) {}
        "#;
        e.run(script).unwrap(); // would hang/deadlock if a lock were held across the comp
    }

    #[test]
    fn mid_fleet_failure_unwinds_with_per_host_capture() {
        // Issue #27: a rolling fleet inside one transaction where host "web2" fails its `docker
        // run` must unwind, and EACH compensation must act on the host it was registered for (the
        // per-host capture). A loop-variable capture bug would `rm` the wrong host's container.
        use crate::engine::runner::FakeRunner;
        let fake = FakeRunner::shared();
        fake.fail_cmd("web2", "docker run", 1, "web2 run failed");
        let ctx = shared(fake.clone()); // LIVE mode
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        crate::engine::builtins::exec::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        let script = r#"
            try {
                transaction(|| {
                    for host in ["web1", "web2"] {
                        let name = "app-" + host;
                        let h = host; let n = name;
                        on_rollback(|| { ssh_exec(h, "rm " + n); });
                        let r = ssh_exec(host, "docker run --name " + name);
                        if !r.ok { throw "run failed on " + host; }
                    }
                });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        let calls = fake.calls();
        // web1's container started, web2's failed -> unwind ran BOTH rm's, each on its OWN host.
        assert!(calls.contains(&"ssh web1: rm app-web1".to_string()), "calls: {calls:?}");
        assert!(calls.contains(&"ssh web2: rm app-web2".to_string()), "calls: {calls:?}");
        // No cross-attribution (the capture bug would `rm app-web2` on web1 or vice versa).
        assert!(!calls.contains(&"ssh web1: rm app-web2".to_string()), "wrong-host capture!");
    }

    #[test]
    fn guarded_compensation_skips_its_destructive_step_when_the_prior_one_fails() {
        // Robustness review R6: lib/deploy.rhai's deploy_one_host registers two compensations —
        // "restore the proxy to the OLD target" (registered second, runs FIRST in the LIFO
        // unwind) and "rm -f the NEW container" (registered first, runs LAST). If the restore
        // compensation itself throws (SSH blip, a health gate rejecting a degraded old
        // container, a bogus old_target), this engine's unwind is best-effort and
        // error-isolated (see `throwing_compensation_does_not_abort_unwind` above) — it logs and
        // KEEPS GOING to the rm-new compensation regardless. Removing the new container in that
        // case blackholes traffic: the proxy never made it back to the old container, so the new
        // one (which the proxy may still be pointing at) could be the only thing serving.
        //
        // The fix is a shared `proxy_restored` flag: the restore compensation sets it only once
        // its own `ssh_exec` call succeeds; the rm-new compensation checks it first and skips the
        // removal otherwise. Rhai closures that reference the SAME outer variable are promoted to
        // a shared cell (empirically confirmed against the real engine, not just documentation),
        // so a write in one closure is visible from the other even though they're invoked at
        // different points during the unwind. This test mirrors deploy.rhai's actual shape (same
        // registration order, same guard) with `ssh_exec` standing in for `docker::docker_remove`
        // / `px_deploy` — ssh_exec is a real builtin whose FakeRunner-observed calls we can assert
        // on directly, without needing the rest of the deploy pipeline (docker/HTTP health
        // checks) that the existing test suite already documents as out of scope for a live run
        // (see docs/robustness-review.md, R8).
        use crate::engine::runner::FakeRunner;
        let fake = FakeRunner::shared();
        fake.fail_cmd("web1", "restore to old_target", 1, "restore failed");
        let ctx = shared(fake.clone()); // LIVE mode
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        crate::engine::builtins::exec::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        let script = r#"
            try {
                transaction(|| {
                    let h = "web1";
                    let nn = "app-new";
                    let ot = "old_target";
                    let nt = "new_target";
                    let proxy_restored = false;

                    on_rollback(|| {
                        if !proxy_restored {
                            return;
                        }
                        ssh_exec(h, "docker rm -f " + nn);
                    });

                    on_rollback(|| {
                        let r = ssh_exec(h, "restore to " + ot);
                        if !r.ok { throw "restore failed on " + h; }
                        proxy_restored = true;
                    });

                    // forward switch (succeeds), then a later host in the fleet fails, unwinding.
                    ssh_exec(h, "switch to " + nt);
                    throw "later host failed";
                });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        let calls = fake.calls();
        assert!(
            calls.contains(&"ssh web1: restore to old_target".to_string()),
            "the restore compensation must have been attempted: {calls:?}"
        );
        assert!(
            !calls.contains(&"ssh web1: docker rm -f app-new".to_string()),
            "the new container must NOT be removed when the proxy restore failed — that would \
             blackhole traffic (R6): {calls:?}"
        );
    }

    #[test]
    fn guarded_compensation_runs_its_destructive_step_when_the_prior_one_succeeds() {
        // The mirror-image, happy-path case of the test above: once the restore compensation
        // succeeds, the rm-new compensation must still actually run (proving the guard doesn't
        // just permanently disable removal — only when the restore genuinely didn't happen).
        use crate::engine::runner::FakeRunner;
        let fake = FakeRunner::shared(); // no fail_cmd — every ssh_exec call succeeds
        let ctx = shared(fake.clone());
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        crate::engine::builtins::exec::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        let script = r#"
            try {
                transaction(|| {
                    let h = "web1";
                    let nn = "app-new";
                    let ot = "old_target";
                    let nt = "new_target";
                    let proxy_restored = false;

                    on_rollback(|| {
                        if !proxy_restored {
                            return;
                        }
                        ssh_exec(h, "docker rm -f " + nn);
                    });

                    on_rollback(|| {
                        let r = ssh_exec(h, "restore to " + ot);
                        if !r.ok { throw "restore failed on " + h; }
                        proxy_restored = true;
                    });

                    ssh_exec(h, "switch to " + nt);
                    throw "later host failed";
                });
            } catch(e) {}
        "#;
        e.run(script).unwrap();
        let calls = fake.calls();
        assert!(
            calls.contains(&"ssh web1: docker rm -f app-new".to_string()),
            "the new container MUST be removed once the proxy restore actually succeeded: {calls:?}"
        );
    }

    #[test]
    fn dry_run_records_rollback_and_does_not_invoke() {
        let ctx = crate::engine::context::shared_dry(FakeRunner::shared());
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
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "rollback"));
    }
}
