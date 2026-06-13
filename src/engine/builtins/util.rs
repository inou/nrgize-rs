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
            if ctx.mode == EffectMode::DryRun {
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

    // json_string(s) -> String — encode `s` as a JSON string literal (with surrounding quotes
    // and all `"`, `\`, control chars escaped). Used by the Caddy module to build admin-API JSON
    // safely instead of splicing raw values into a hand-written JSON string (issue #10): a domain
    // or target containing `"` can no longer break out of the JSON.
    engine.register_fn("json_string", |s: &str| -> String {
        serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
    });

    // to_json(value) / from_json(text) — round-trip a Rhai value (map/array/string/int/bool)
    // through JSON. deploy() uses these to persist the FULL effective deploy config in state so
    // rollback() can replay it verbatim (issue #6) instead of reverting envs/port/health to
    // defaults. Numbers come back as integers/floats; nested maps/arrays are preserved.
    engine.register_fn("to_json", |v: rhai::Dynamic| -> String {
        serde_json::to_string(&v).unwrap_or_else(|_| "null".to_string())
    });
    engine.register_fn(
        "from_json",
        |text: &str| -> Result<rhai::Dynamic, Box<EvalAltResult>> {
            serde_json::from_str::<rhai::Dynamic>(text)
                .map_err(|e| format!("from_json: invalid JSON: {e}").into())
        },
    );

    // url_encode(s) -> String — percent-encode `s` for safe use inside a URL component (e.g. a DB
    // password in `postgres://user:<pw>@host`). Encodes everything that is not an RFC 3986
    // unreserved character. The example deploy files use it so a password with `@`, `:`, `/`, or
    // `#` doesn't corrupt the connection string.
    engine.register_fn("url_encode", |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
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

    #[test]
    fn json_string_escapes_quotes() {
        let e = engine();
        let v: String = e.eval(r#"json_string("a\"b")"#).unwrap();
        assert_eq!(v, r#""a\"b""#);
    }

    #[test]
    fn url_encode_escapes_reserved_chars() {
        let e = engine();
        let v: String = e.eval(r#"url_encode("p@ss:w/rd#1")"#).unwrap();
        assert_eq!(v, "p%40ss%3Aw%2Frd%231");
        // Unreserved chars pass through untouched.
        assert_eq!(e.eval::<String>(r#"url_encode("aZ0-_.~")"#).unwrap(), "aZ0-_.~");
    }

    #[test]
    fn to_from_json_round_trips_a_config_map() {
        // The shape deploy() persists for rollback: a nested map with strings + ints.
        let e = engine();
        let ok: bool = e
            .eval(
                r#"
                let cfg = #{ container_port: 3000, health_path: "/up",
                             envs: #{ "DATABASE_URL": "postgres://x", "N": "1" } };
                let back = from_json(to_json(cfg));
                back.container_port == 3000 && back.health_path == "/up"
                    && back.envs.DATABASE_URL == "postgres://x"
            "#,
            )
            .unwrap();
        assert!(ok);
    }
}
