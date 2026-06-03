//! Small utility builtins.

use crate::engine::context::SharedCtx;
use rhai::{Engine, EvalAltResult};

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    use crate::engine::context::EffectMode;
    {
        let ctx = ctx.clone();
        engine.register_fn("sleep", move |seconds: i64| {
            if ctx.lock().unwrap().mode == EffectMode::DryRun {
                return; // don't actually sleep in dry-run
            }
            if seconds > 0 {
                std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
            }
        });
    }

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
        let v: String = e
            .eval(r#"env_or("NRG_DEFINITELY_UNSET_XYZ", "fallback")"#)
            .unwrap();
        assert_eq!(v, "fallback");
    }

    #[test]
    fn nrg_env_throws_when_unset() {
        let e = engine();
        let r = e.eval::<String>(r#"nrg_env("NRG_DEFINITELY_UNSET_XYZ")"#);
        assert!(r.is_err());
    }
}
