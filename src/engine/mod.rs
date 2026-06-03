//! Rhai-powered orchestration engine (replaces the Starlark runtime).
pub mod builtins;
pub mod context;
pub mod eval;
pub mod plan;
pub mod runner;
pub mod secret;
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

    // Route print/debug through secret redaction so a script that echoes or reveal()s a secret
    // into output can't leak it (defense-in-depth; the Secret type is the primary guard).
    let secrets = ctx.lock().unwrap().secrets.clone();
    let sp = secrets.clone();
    engine.on_print(move |s| eprintln!("{}", secret::redact(s, &sp.lock().unwrap())));
    let sd = secrets;
    engine.on_debug(move |s, _src, pos| {
        eprintln!("[debug] {} @ {pos:?}", secret::redact(s, &sd.lock().unwrap()))
    });

    types::register_types(&mut engine);
    builtins::register_builtins(&mut engine, ctx.clone());
    secret::register(&mut engine, ctx.clone());
    transaction::register(&mut engine, ctx);
    engine
}
