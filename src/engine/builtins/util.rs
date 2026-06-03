//! Small utility builtins.

use crate::engine::context::SharedCtx;
use rhai::{Array, Engine, EvalAltResult};

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    use crate::engine::context::EffectMode;

    // join(array, sep) -> String. Rhai has no Array::join; the ported stdlib uses this to build
    // `--build-arg k=v` / `-p`/`-e` token lists and ", "-joined failed-host messages. Each
    // element is stringified (string elements pass through; numbers/bools coerce).
    engine.register_fn("join", |arr: Array, sep: &str| -> String {
        arr.iter()
            .map(|d| {
                d.clone()
                    .into_string()
                    .unwrap_or_else(|_| d.to_string())
            })
            .collect::<Vec<_>>()
            .join(sep)
    });
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

    #[test]
    fn join_concatenates_string_elements_with_separator() {
        let e = engine();
        let v: String = e.eval(r#"join(["-p", "80:80", "-p", "443:443"], " ")"#).unwrap();
        assert_eq!(v, "-p 80:80 -p 443:443");
    }

    #[test]
    fn join_handles_empty_and_single() {
        let e = engine();
        assert_eq!(e.eval::<String>(r#"join([], ", ")"#).unwrap(), "");
        assert_eq!(e.eval::<String>(r#"join(["only"], ", ")"#).unwrap(), "only");
    }

    #[test]
    fn join_stringifies_non_string_elements() {
        let e = engine();
        let v: String = e.eval(r#"join([1, 2, 3], "-")"#).unwrap();
        assert_eq!(v, "1-2-3");
    }
}
