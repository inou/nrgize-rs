//! Rhai-powered orchestration engine (replaces the Starlark runtime).
pub mod builtins;
pub mod context;
pub mod eval;
pub mod runner;
pub mod state;
pub mod types;

use crate::engine::context::SharedCtx;
use rhai::Engine;

/// Build an engine with result types + all builtins registered, `print`/`debug` routed
/// to stderr, and trusted-script safety limits lifted. The module resolver is set
/// per-file in `eval::run_file`.
pub fn build_engine(ctx: SharedCtx) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(0); // trusted scripts: unlimited
    engine.on_print(|s| eprintln!("{s}"));
    engine.on_debug(|s, _src, pos| eprintln!("[debug] {s} @ {pos:?}"));
    types::register_types(&mut engine);
    builtins::register_builtins(&mut engine, ctx);
    engine
}
