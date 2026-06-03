//! Persistent-state builtins, backed by the RunCtx's StateStore. Reads/writes snapshot the
//! store Arc out of the RunCtx lock before touching it (so disk I/O never holds RunCtx).

use crate::engine::context::SharedCtx;
use crate::engine::state::StateStore;
use rhai::{Dynamic, Engine, EvalAltResult, Map};
use std::sync::{Arc, Mutex};

fn store(ctx: &SharedCtx) -> Arc<Mutex<StateStore>> {
    ctx.lock().unwrap().state.clone()
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // state_get(key) -> String | ()   (() when absent, so scripts can use `if x { }`)
    {
        let ctx = ctx.clone();
        engine.register_fn("state_get", move |key: &str| -> Dynamic {
            match store(&ctx).lock().unwrap().get(key) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
    }
    // state_set(key, value) — persists atomically; throws on I/O failure.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_set",
            move |key: &str, value: &str| -> Result<(), Box<EvalAltResult>> {
                store(&ctx)
                    .lock()
                    .unwrap()
                    .set(key, value)
                    .map_err(|e| e.into())
            },
        );
    }
    // state_del(key) — persists atomically; throws on I/O failure.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_del",
            move |key: &str| -> Result<(), Box<EvalAltResult>> {
                store(&ctx).lock().unwrap().del(key).map_err(|e| e.into())
            },
        );
    }
    // state_all() -> Map
    {
        let ctx = ctx.clone();
        engine.register_fn("state_all", move || -> Map {
            store(&ctx)
                .lock()
                .unwrap()
                .all()
                .into_iter()
                .map(|(k, v)| (k.into(), Dynamic::from(v)))
                .collect()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared_with_state;
    use crate::engine::runner::FakeRunner;

    fn engine_with_disk(root: &std::path::Path) -> (Engine, SharedCtx) {
        let store = StateStore::load(root).unwrap();
        let ctx = shared_with_state(FakeRunner::shared(), store);
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        (e, ctx)
    }

    #[test]
    fn set_get_del_roundtrip_in_script() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let out: String = e
            .eval(
                r#"
                state_set("app.version", "v9");
                let v = state_get("app.version");
                state_del("missing-is-fine");
                v
            "#,
            )
            .unwrap();
        assert_eq!(out, "v9");
        // Persisted to disk by the atomic flush.
        let reloaded = StateStore::load(tmp.path()).unwrap();
        assert_eq!(reloaded.get("app.version"), Some("v9".to_string()));
    }

    #[test]
    fn state_get_absent_is_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let present: bool = e
            .eval(r#"if state_get("nope") == () { false } else { true }"#)
            .unwrap();
        assert!(!present);
    }
}
