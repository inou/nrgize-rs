//! Persistent-state builtins, backed by the RunCtx's StateStore. Reads/writes snapshot the
//! store Arc out of the RunCtx lock before touching it (so disk I/O never holds RunCtx).

use crate::engine::context::{EffectMode, SharedCtx};
use crate::engine::state::StateStore;
use rhai::{Dynamic, Engine, EvalAltResult, Map};
use std::sync::{Arc, Mutex};

fn store(ctx: &SharedCtx) -> Arc<Mutex<StateStore>> {
    ctx.lock().unwrap().state.clone()
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // state_get(key) -> String | ()   (() when absent). NOTE: Rhai requires `bool` conditions,
    // so test presence with `state_get(k) != ()` or `has_state(k)` — NOT `if state_get(k) {}`,
    // which raises a runtime type error.
    {
        let ctx = ctx.clone();
        engine.register_fn("state_get", move |key: &str| -> Dynamic {
            match store(&ctx).lock().unwrap().get(key) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
    }
    // has_state(key) -> bool — ergonomic presence check (Rhai conditions must be bool).
    {
        let ctx = ctx.clone();
        engine.register_fn("has_state", move |key: &str| -> bool {
            store(&ctx).lock().unwrap().get(key).is_some()
        });
    }
    // state_set(key, value) — persists atomically; records (not executes) in dry-run, where
    // the store is an overlay (no-flush), so state_get stays consistent.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_set",
            move |key: &str, value: &str| -> Result<(), Box<EvalAltResult>> {
                let snap = ctx.lock().unwrap().snapshot();
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("state", None, format!("{key} = {value}"));
                }
                let result = snap.state.lock().unwrap().set(key, value); // guard drops here
                result.map_err(|e| e.into())
            },
        );
    }
    // state_del(key) — persists atomically; records in dry-run.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_del",
            move |key: &str| -> Result<(), Box<EvalAltResult>> {
                let snap = ctx.lock().unwrap().snapshot();
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("state", None, format!("del {key}"));
                }
                let result = snap.state.lock().unwrap().del(key); // guard drops here
                result.map_err(|e| e.into())
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

    #[test]
    fn has_state_reports_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let r: bool = e
            .eval(r#"state_set("k", "v"); has_state("k") && !has_state("nope")"#)
            .unwrap();
        assert!(r);
    }
}
