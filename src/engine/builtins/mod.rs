//! Registration of all side-effecting Rhai builtins.
pub mod exec;
pub mod http;
pub mod preflight;
pub mod sim;
pub mod state;
pub mod steps;
pub mod transfer;
pub mod util;

use crate::engine::context::SharedCtx;
use rhai::Engine;

/// Register every builtin into the engine, each capturing the shared context.
pub fn register_builtins(engine: &mut Engine, ctx: SharedCtx) {
    exec::register(engine, ctx.clone());
    steps::register(engine, ctx.clone());
    transfer::register(engine, ctx.clone());
    preflight::register(engine, ctx.clone());
    http::register(engine, ctx.clone());
    sim::register(engine, ctx.clone());
    state::register(engine, ctx.clone());
    util::register(engine, ctx);
}
