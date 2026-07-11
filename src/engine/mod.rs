//! Rhai-powered orchestration engine (replaces the Starlark runtime).
pub mod builtins;
pub mod context;
pub mod eval;
pub mod interrupt;
pub mod plan;
pub mod runner;
pub mod secret;
pub mod sim;
pub mod state;
pub mod transaction;
pub mod types;

use crate::engine::context::SharedCtx;
use rhai::Engine;

/// Build an engine with result types + all builtins registered, `print`/`debug` routed
/// to stderr, and trusted-script safety limits lifted. The module resolver is set
/// per-file in `eval::run_file`.
pub fn build_engine(ctx: SharedCtx) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(0); // trusted scripts: unlimited
    // Trusted scripts: lift the expression-nesting cap too. The stdlib builds long
    // `a + b + c + ...` command/message strings and deep `if cfg.contains(k) {..} else {..}`
    // config chains whose ASTs exceed Rhai's default function-body depth of 32.
    engine.set_max_expr_depths(0, 0);
    // Robustness review R8b: `set_max_expr_depths` above only lifts the *expression*-nesting cap
    // — Rhai's SEPARATE function-*call*-nesting cap (`max_call_levels`) was never touched here, so
    // it silently stayed at Rhai's own default: just 8 levels in a debug build (64 in release —
    // `rhai::api::limits::default_limits::MAX_CALL_STACK_DEPTH`). This codebase's own stdlib
    // routinely nests deeper than that: e.g. `rollback()` (2-arg) -> `rollback()` (3-arg) ->
    // `deploy()` -> its `transaction()` closure -> `deploy_one_host()` -> `wait_healthy_on_host()`
    // -> its private `ssh_http_status()` helper is 7 script-function levels before any host work
    // even starts, and every debug build (`cargo test`, `cargo build` without `--release`) — this
    // whole test suite included — runs at the 8-level default. A live end-to-end `rollback()` call
    // (this file's own `rollback_happy_path_...` test) reliably tripped Rhai's `ErrorStackOverflow`
    // BEFORE this fix, entirely from ordinary, non-recursive call nesting — not a runaway/infinite
    // recursion bug. Found only once a test finally exercised rollback() live end-to-end (R8b), the
    // exact "no test, so nobody notices until an incident" gap that finding called out.
    //
    // Deliberately raised to Rhai's OWN release-build default (64), not higher: an adversarial
    // review (this fix's own Opus pass) confirmed empirically that a genuinely infinite/runaway
    // script recursion hits this cap as a clean, catchable `ErrorStackOverflow` at 64 on EVERY
    // thread stack size tried, but at 128+ it instead hard-aborts the whole process
    // (`SIGABRT`, bypassing `transaction()`'s unwind entirely — no compensations run) on a 2 MiB
    // stack, which is Rust's default for spawned/test threads and so applies to this entire test
    // suite. 64 keeps ~5-8x headroom over the deepest legitimate chain in this stdlib (rollback's
    // own indirection above, or `standard_deploy` -> `deploy()` -> ... -> the Caddy proxy path)
    // while staying inside the size Rhai's own release default already treats as safe everywhere.
    engine.set_max_call_levels(64);

    // Route print/debug through secret redaction so a script that echoes or reveal()s a secret
    // into output can't leak it (defense-in-depth; the Secret type is the primary guard).
    let secrets = ctx.secrets.clone();
    let sp = secrets.clone();
    engine.on_print(move |s| eprintln!("{}", secret::redact(s, &sp.lock().unwrap())));
    let sd = secrets;
    engine.on_debug(move |s, _src, pos| {
        eprintln!("[debug] {} @ {pos:?}", secret::redact(s, &sd.lock().unwrap()))
    });

    // R7: poll the interrupt flag between operations. A set flag ends the running script with a
    // normal `Err` (`ErrorTerminated`) — the exact path a `throw` takes — so an enclosing
    // transaction()'s existing unwind machinery runs every registered compensation before the
    // process exits, instead of Ctrl-C killing it with zero cleanup. See
    // src/engine/interrupt.rs for exactly what this can and can't preempt.
    //
    // `swap(false, ...)` CONSUMES the interrupt: the first check after the flag is set both
    // terminates whatever's currently running AND clears it, so the on_rollback compensations
    // that run during the unwind aren't immediately re-terminated by the same still-set flag
    // (that would silently turn every compensation into a no-op — the exact failure mode this
    // fix exists to prevent). A repeat Ctrl-C during the unwind, arriving AFTER this swap has
    // already cleared the flag, sets it again and is caught the same way here — cutting the
    // currently running compensation short. That's distinct from the OS-level force-quit escape
    // hatch in interrupt.rs: a repeat signal that arrives BEFORE this poll consumes the first one
    // exits the process immediately, bypassing this check entirely.
    let interrupted = ctx.interrupted.clone();
    engine.on_progress(move |_ops| {
        if interrupted.swap(false, std::sync::atomic::Ordering::Relaxed) {
            Some("Interrupted (SIGINT/SIGTERM)".into())
        } else {
            None
        }
    });

    types::register_types(&mut engine);
    builtins::register_builtins(&mut engine, ctx.clone());
    secret::register(&mut engine, ctx.clone());
    transaction::register(&mut engine, ctx);
    engine
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// Fast, no-real-signal coverage of the R7 wiring: a test-only `simulate_interrupt()`
    /// builtin sets `ctx.interrupted` exactly like the real SIGINT/SIGTERM handler would, then
    /// asserts (a) the running script aborts at the VERY NEXT operation with `ErrorTerminated`,
    /// and (b) the enclosing transaction's on_rollback compensation still runs — proving the
    /// flag was CONSUMED, not left set to re-terminate the compensation too (the exact bug this
    /// test caught during development: without `swap`, the compensation's own `log("undo")` call
    /// was itself immediately re-terminated, silently turning every rollback into a no-op).
    ///
    /// Note: `ErrorTerminated` is deliberately NOT catchable by a script-level `try`/`catch` —
    /// Rhai treats it as an externally-imposed abort, not a normal exception, so it propagates
    /// all the way out of `engine.run()` regardless of any `try` wrapping the `transaction()`
    /// call. `transaction()`'s own unwind is separate: a native Rust `match` on the `Result`
    /// from calling the body, not Rhai-level `catch` semantics — which is why the compensation
    /// still runs even though nothing in the script "catches" anything. The real-signal,
    /// real-process end-to-end path is covered by tests/interrupt.rs.
    #[test]
    fn interrupt_flag_aborts_the_script_and_the_compensation_still_runs() {
        let ctx = shared(FakeRunner::shared());
        let mut engine = build_engine(ctx.clone());

        let flag = ctx.interrupted.clone();
        engine.register_fn("simulate_interrupt", move || flag.store(true, Ordering::Relaxed));
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        engine.register_fn("log", move |s: &str| l.lock().unwrap().push(s.to_string()));

        let script = r#"
            transaction(|| {
                on_rollback(|| log("undo"));
                simulate_interrupt();
                log("should not run");
            });
        "#;
        let result = engine.run(script);
        let err = result.expect_err("an interrupted script must surface as an Err");
        assert!(
            matches!(*err, rhai::EvalAltResult::ErrorTerminated(..)),
            "expected ErrorTerminated, got: {err:?}"
        );

        let entries = log.lock().unwrap().clone();
        assert!(entries.contains(&"undo".to_string()), "compensation must have run: {entries:?}");
        assert!(
            !entries.contains(&"should not run".to_string()),
            "the script must abort at the next operation after the interrupt: {entries:?}"
        );
    }
}
