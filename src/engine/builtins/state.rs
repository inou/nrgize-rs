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
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_update",
            move |values: Map| -> Result<(), Box<EvalAltResult>> {
                let values = values
                    .into_iter()
                    .map(|(k, v)| {
                        Ok((
                            k.to_string(),
                            v.into_string()
                                .map_err(|_| "state values must be strings")?,
                        ))
                    })
                    .collect::<Result<std::collections::BTreeMap<_, _>, Box<EvalAltResult>>>()?;
                if ctx.is_dry_run() {
                    for (key, value) in &values {
                        ctx.record(
                            "state",
                            None,
                            format!("{key} = <{} bytes> (atomic batch)", value.len()),
                        );
                    }
                }
                ctx.state
                    .lock()
                    .unwrap()
                    .update(values, &[])
                    .map_err(Into::into)
            },
        );
    }
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
                    // The VALUE never reaches the plan, which prints to stdout (and therefore into
                    // CI logs and terminal scrollback). `RunCtx::record`'s redaction is
                    // SUBSTRING-based, so it only catches a secret stored verbatim: a value
                    // DERIVED from one — `url_encode(reveal(secret("DB_PASSWORD")))` inside a
                    // DATABASE_URL, or the whole `to_json(cfg)` blob `deploy()` persists — no
                    // longer contains the registered plaintext and would print in the clear.
                    // Record the key plus a byte count instead, the same shape `write_remote`
                    // already uses for its (equally sensitive) body.
                    ctx.record("state", None, format!("{key} = <{} bytes>", value.len()));
                }
                ctx.state
                    .lock()
                    .unwrap()
                    .set(key, value)
                    .map_err(|e| e.into())
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
    // nrg_dest() -> String — the active destination (roadmap 2.2), e.g. "staging", or
    // "default" when `nrg exec`/`nrg run` was invoked without `--dest`. Lets a script branch on
    // its own destination (e.g. to pick a different domain/replica count) without needing to
    // parse `env::args()` or duplicate the CLI's own default-name convention.
    {
        let ctx = ctx.clone();
        engine.register_fn("nrg_dest", move || -> String {
            store(&ctx)
                .lock()
                .unwrap()
                .dest()
                .unwrap_or_else(|| "default".to_string())
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
            ctx.session
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
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
        assert_eq!(plan[0].detail, "token = <16 bytes>");
        assert!(!plan[0].detail.contains("supersecretvalue"));
    }

    #[test]
    fn dry_run_state_plan_omits_a_value_derived_from_a_secret() {
        // The redaction gap this closes: a value TRANSFORMED from a secret (here percent-encoded,
        // as the shipped example does for a DB password inside a DATABASE_URL) no longer CONTAINS
        // the registered plaintext, so `record`'s substring redaction cannot match it. The plan
        // detail carries the key and a byte count only, so no value — derived or not — reaches
        // stdout.
        use crate::engine::context::{shared_with_state, EffectMode};
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::load_overlay(tmp.path()).unwrap();
        let ctx = shared_with_state(FakeRunner::shared(), store, EffectMode::DryRun);
        ctx.register_secret("p@ssw0rd#1");
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        let value = "postgres://app:p%40ssw0rd%231@db:5432/app_production";
        e.run(&format!(r#"state_set("app.config", "{value}");"#))
            .unwrap();
        let plan = ctx.plan.lock().unwrap().clone();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].detail,
            format!("app.config = <{} bytes>", value.len())
        );
        assert!(
            !plan[0].detail.contains("p%40ssw0rd%231"),
            "encoded secret leaked into the plan"
        );
        // The stored value itself is untouched — only the PLAN text elides it.
        assert_eq!(
            ctx.state.lock().unwrap().get("app.config").as_deref(),
            Some(value)
        );
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
        e.run(r#"session_set("nrg.runtime.cmd", "podman");"#)
            .unwrap();
        // Nothing was ever written to .energize/ — session_set has no disk side effect at all.
        assert!(!tmp.path().join(".energize").join("state.json").exists());
        // A fresh StateStore load (simulating a later, separate invocation) never sees it.
        let reloaded = StateStore::load(tmp.path()).unwrap();
        assert_eq!(reloaded.get("nrg.runtime.cmd"), None);
    }

    #[test]
    fn nrg_dest_reports_default_when_no_destination_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let dest: String = e.eval(r#"nrg_dest()"#).unwrap();
        assert_eq!(dest, "default");
    }

    #[test]
    fn nrg_dest_reports_the_active_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::load(tmp.path())
            .unwrap()
            .with_dest(Some("staging".to_string()));
        let ctx = shared_with_state(FakeRunner::shared(), store, EffectMode::Live);
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        let dest: String = e.eval(r#"nrg_dest()"#).unwrap();
        assert_eq!(dest, "staging");
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
