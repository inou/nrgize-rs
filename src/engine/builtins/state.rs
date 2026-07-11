//! Persistent-state builtins, backed by the RunCtx's StateStore. Reads/writes snapshot the
//! store Arc out of the RunCtx lock before touching it (so disk I/O never holds RunCtx).

use crate::engine::context::{EffectMode, SharedCtx};
use crate::engine::state::StateStore;
use rhai::{Dynamic, Engine, EvalAltResult, Map};
use std::sync::{Arc, Mutex};

fn store(ctx: &SharedCtx) -> Arc<Mutex<StateStore>> {
    ctx.state.clone()
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
                if ctx.mode == EffectMode::DryRun {
                    ctx.record("state", None, format!("{key} = {value}"));
                }
                ctx.state.lock().unwrap().set(key, value).map_err(|e| e.into())
            },
        );
    }
    // state_del(key) — persists atomically; records in dry-run.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_del",
            move |key: &str| -> Result<(), Box<EvalAltResult>> {
                if ctx.mode == EffectMode::DryRun {
                    ctx.record("state", None, format!("del {key}"));
                }
                ctx.state.lock().unwrap().del(key).map_err(|e| e.into())
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
    // session_set(key, value) / session_get(key) / has_session(key) — an EPHEMERAL, in-memory
    // key/value store distinct from state_set/state_get: it never touches disk, so a value set
    // here is visible to every module `import`ed within THIS run (the one thing `state_set` was
    // originally repurposed for by `lib/runtime.rhai`) without becoming durable and leaking into
    // a later, unrelated invocation (robustness review R27). Same semantics across Live/DryRun —
    // there is nothing to redact or record, since nothing is ever persisted or planned.
    {
        let ctx = ctx.clone();
        engine.register_fn("session_set", move |key: &str, value: &str| {
            ctx.session.lock().unwrap().insert(key.to_string(), value.to_string());
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("session_get", move |key: &str| -> Dynamic {
            match ctx.session.lock().unwrap().get(key) {
                Some(v) => Dynamic::from(v.clone()),
                None => Dynamic::UNIT,
            }
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("has_session", move |key: &str| -> bool {
            ctx.session.lock().unwrap().contains_key(key)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared_with_state;
    use crate::engine::runner::FakeRunner;

    fn engine_with_disk(root: &std::path::Path) -> (Engine, SharedCtx) {
        use crate::engine::context::EffectMode;
        let store = StateStore::load(root).unwrap();
        let ctx = shared_with_state(FakeRunner::shared(), store, EffectMode::Live);
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
    fn dry_run_state_plan_is_redacted() {
        use crate::engine::context::{shared_with_state, EffectMode};
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::load_overlay(tmp.path()).unwrap();
        let ctx = shared_with_state(FakeRunner::shared(), store, EffectMode::DryRun);
        ctx.register_secret("supersecretvalue");
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        // A reveal()'d secret stored to state must NOT appear in the (stdout) plan.
        e.run(r#"state_set("token", "supersecretvalue");"#).unwrap();
        let plan = ctx.plan.lock().unwrap().clone();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].detail, "token = ***");
    }

    #[test]
    fn session_set_get_has_roundtrip_in_script() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let ok: bool = e
            .eval(
                r#"
                session_set("nrg.runtime.cmd", "podman");
                session_get("nrg.runtime.cmd") == "podman"
                    && has_session("nrg.runtime.cmd")
                    && !has_session("nope")
            "#,
            )
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn session_get_absent_is_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let present: bool = e
            .eval(r#"if session_get("nope") == () { false } else { true }"#)
            .unwrap();
        assert!(!present);
    }

    #[test]
    fn session_set_never_touches_disk() {
        // Robustness review R27: session_set/get must be purely in-memory, even when the
        // context is backed by a REAL on-disk project (not the ephemeral test store), and even
        // though it shares a key namespace with state_set (e.g. "nrg.runtime.cmd").
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        e.run(r#"session_set("nrg.runtime.cmd", "podman");"#).unwrap();
        // Nothing was ever written to .energize/ — session_set has no disk side effect at all.
        assert!(!tmp.path().join(".energize").join("state.json").exists());
        // A fresh StateStore load (simulating a later, separate invocation) never sees it.
        let reloaded = StateStore::load(tmp.path()).unwrap();
        assert_eq!(reloaded.get("nrg.runtime.cmd"), None);
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
