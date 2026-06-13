//! Tagged secret values + POSIX-safe quoting + trace redaction.

use crate::engine::context::SharedCtx;
use rhai::{Engine, EvalAltResult};
use std::collections::HashSet;

/// Secrets shorter than this are rejected: substring redaction can't safely distinguish a
/// very short secret from ordinary output (and a too-short secret is weak anyway).
pub const MIN_SECRET_LEN: usize = 6;

/// A secret value. Deliberately NOT convertible to `String` in scripts: the only ways to get
/// the plaintext are `reveal()` / `sh_quote()`, so every plaintext use is explicit.
#[derive(Clone)]
pub struct Secret(String);

// Hand-written so Rust-side `{:?}` (error messages, container debug) can NEVER print the
// plaintext — a derived Debug would emit `Secret("plaintext")`.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

/// POSIX single-quote escaping: wrap in `'…'`, and render any embedded `'` as `'\''`.
/// Safe for newlines, `$`, backticks, spaces — everything stays literal.
pub fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Look up a secret by name: `NRG_SECRET_<UPPER>` env var, then `<root>/.energize/secrets`, then
/// `<root>/.env` (both `KEY=VALUE`, optional surrounding quotes). `root` is the discovered project
/// root (the same anchor as state); `None` falls back to CWD-relative paths (ephemeral/tests).
/// Resolving against `root` (not CWD) means running `nrg` from a subdirectory still finds the
/// project's secrets instead of silently missing them (issue #19).
pub fn lookup_secret(root: Option<&std::path::Path>, name: &str) -> Option<String> {
    let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return Some(v);
    }
    for rel in [".energize/secrets", ".env"] {
        let path = match root {
            Some(r) => r.join(rel),
            None => std::path::PathBuf::from(rel),
        };
        if let Some(v) = load_from_kv_file(&path, name) {
            return Some(v);
        }
    }
    None
}

fn load_from_kv_file(path: &std::path::Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(v);
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Replace every registered secret value in `text` with `***`. Defense-in-depth for any
/// output sink; the primary protection is the `Secret` type itself. Substring-based, so it
/// can't catch a secret that was transformed (e.g. base64) before reaching `text` — that's an
/// accepted limit of the redaction layer (see the spec's "accepted tradeoffs").
pub fn redact(text: &str, secrets: &HashSet<String>) -> String {
    // Longest-first (then lexical) for deterministic results when one secret is a substring
    // of another — `HashSet` iteration order is otherwise nondeterministic.
    let mut vals: Vec<&String> = secrets.iter().filter(|s| s.len() >= MIN_SECRET_LEN).collect();
    vals.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.as_str().cmp(b.as_str())));
    let mut out = text.to_string();
    for s in vals {
        out = out.replace(s.as_str(), "***");
    }
    out
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    engine
        .register_type_with_name::<Secret>("Secret")
        // `to_string` is the path Rhai string INTERPOLATION uses (`` `... ${secret("PW")} ...` ``).
        // We CANNOT just throw here: Rhai swallows a `to_string` error during interpolation and
        // silently falls back to the bare type name ("Secret"), so the command would still be
        // built and run with the wrong value. Instead we emit a unique, non-shell SENTINEL so the
        // value is harmless if executed AND detectable: every command-executing builtin runs
        // `assert_no_secret_leak()` and throws if the sentinel is present (see builtins/exec.rs).
        // Both the by-`&mut` (method-call) and by-value (interpolation) forms must be registered.
        // `to_debug` (used by `debug()` and container Debug) still renders "***".
        .register_fn("to_string", |_s: &mut Secret| SECRET_SENTINEL.to_string())
        .register_fn("to_string", |_s: Secret| SECRET_SENTINEL.to_string())
        .register_fn("to_debug", |_s: &mut Secret| "***".to_string());

    // secret(name) -> Secret  (throws if missing or too short)
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "secret",
            move |name: &str| -> Result<Secret, Box<EvalAltResult>> {
                // Resolve secret files against the project root (same anchor as state), not CWD.
                let root = ctx.state.lock().unwrap().root();
                let value = lookup_secret(root.as_deref(), name).ok_or_else(|| -> Box<EvalAltResult> {
                    format!(
                        "secret '{name}' not found (checked $NRG_SECRET_{}, .energize/secrets, .env)",
                        name.to_uppercase()
                    )
                    .into()
                })?;
                if value.len() < MIN_SECRET_LEN {
                    return Err(format!(
                        "secret '{name}' is too short ({} chars); secrets must be at least {} chars",
                        value.len(),
                        MIN_SECRET_LEN
                    )
                    .into());
                }
                // Register the plaintext for redaction.
                ctx.secrets.lock().unwrap().insert(value.clone());
                Ok(Secret::new(value))
            },
        );
    }

    // reveal(secret) -> String   (explicit un-wrap)
    engine.register_fn("reveal", |s: Secret| -> String { s.reveal().to_string() });

    // sh_quote(x) -> String   for both String and Secret (the only safe interpolation path)
    engine.register_fn("sh_quote", |s: &str| -> String { posix_quote(s) });
    engine.register_fn("sh_quote", |s: Secret| -> String { posix_quote(s.reveal()) });

    // Forbid string concatenation with a Secret. Rhai would otherwise auto-stringify it via
    // to_string() (= "***"), silently producing a broken `... + ***` command. Failing loudly
    // forces sh_quote(secret) for shell args or reveal(secret) for explicit plaintext.
    engine.register_fn("+", |_a: &str, _b: Secret| -> Result<String, Box<EvalAltResult>> {
        Err(NO_CONCAT.into())
    });
    engine.register_fn("+", |_a: Secret, _b: &str| -> Result<String, Box<EvalAltResult>> {
        Err(NO_CONCAT.into())
    });
    engine.register_fn("+", |_a: Secret, _b: Secret| -> Result<String, Box<EvalAltResult>> {
        Err(NO_CONCAT.into())
    });
}

const NO_CONCAT: &str = "refusing to concatenate a Secret into a string; use sh_quote(secret) \
                         for a shell argument or reveal(secret) for explicit plaintext";

/// The marker a `Secret` stringifies to (via interpolation or `to_string()`). It is wrapped in
/// control characters so it can never collide with a legitimate command and is trivially
/// detectable. A command containing it never reaches a host: `assert_no_secret_leak` throws.
pub const SECRET_SENTINEL: &str = "\u{1}NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\u{1}";

/// Reject a command that contains a stringified `Secret` (the `SECRET_SENTINEL`). This is the
/// command-boundary half of the interpolation guard: `secret()` returns a value that throws on
/// `+` and stringifies to the sentinel, and every effectful builtin calls this before executing
/// or recording the command, so a `${secret(...)}` that slipped through to_string fails loudly.
pub fn assert_no_secret_leak(cmd: &str) -> Result<(), Box<EvalAltResult>> {
    if cmd.contains(SECRET_SENTINEL) {
        return Err(NO_INTERP.into());
    }
    Ok(())
}

const NO_INTERP: &str = "a Secret was string-converted into a command (e.g. via interpolation \
                         `${secret}` or to_string()); use sh_quote(secret) for a shell argument \
                         or reveal(secret) for explicit plaintext";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn posix_quote_is_injection_safe() {
        assert_eq!(posix_quote("simple"), "'simple'");
        assert_eq!(posix_quote("a b$c`d"), "'a b$c`d'"); // metachars stay literal
        assert_eq!(posix_quote("it's"), "'it'\\''s'"); // embedded quote
        assert_eq!(posix_quote("line1\nline2"), "'line1\nline2'"); // newline preserved
    }

    #[test]
    fn redact_replaces_known_secrets_only() {
        let mut s = HashSet::new();
        s.insert("supersecretvalue".to_string());
        s.insert("ab".to_string()); // too short — must NOT be used (false-positive guard)
        let out = redact("token=supersecretvalue and ab and abacus", &s);
        assert_eq!(out, "token=*** and ab and abacus");
    }

    #[test]
    fn secret_reveals_only_via_method() {
        let s = Secret::new("hunter2pw".to_string());
        assert_eq!(s.reveal(), "hunter2pw");
    }

    #[test]
    fn debug_does_not_leak_plaintext() {
        let s = Secret::new("plaintextsecret".to_string());
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{:?}", vec![s]), "[Secret(***)]"); // also via container Debug
    }

    fn secret_engine() -> Engine {
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        register(&mut e, ctx);
        e
    }

    #[test]
    fn secret_to_string_yields_sentinel_and_no_string_coercion() {
        std::env::set_var("NRG_SECRET_DEMO", "topsecretvalue");
        let e = secret_engine();
        // to_string() must NOT return the plaintext nor a plausible value — it yields the
        // sentinel, which the exec boundary rejects (so it can't silently feed a command).
        let s: String = e.eval(r#"secret("DEMO").to_string()"#).unwrap();
        assert_eq!(s, SECRET_SENTINEL);
        assert!(!s.contains("topsecretvalue"));
        // string concat with a Secret must NOT eval (no `+` operator registered).
        assert!(e.eval::<String>(r#""x" + secret("DEMO")"#).is_err());
        std::env::remove_var("NRG_SECRET_DEMO");
    }

    #[test]
    fn secret_interpolation_is_caught_by_the_boundary_guard() {
        // The bug: `${secret(...)}` stringifies the Secret, and Rhai swallows a to_string error
        // during interpolation, so we cannot throw there. Instead it yields the sentinel, which
        // `assert_no_secret_leak` (run by every effectful builtin) rejects.
        std::env::set_var("NRG_SECRET_INTERP", "hunter2value");
        let e = secret_engine();
        let cmd: String = e.eval(r#"`docker login -p ${secret("INTERP")}`"#).unwrap();
        assert!(cmd.contains(SECRET_SENTINEL), "interpolation must yield the sentinel");
        assert!(!cmd.contains("hunter2value"), "interpolation must not leak plaintext");
        assert!(assert_no_secret_leak(&cmd).is_err(), "boundary guard must reject the sentinel");
        // The supported paths still work and pass the guard.
        let quoted: String = e.eval(r#"`docker login -p ${sh_quote(secret("INTERP"))}`"#).unwrap();
        assert_eq!(quoted, "docker login -p 'hunter2value'");
        assert!(assert_no_secret_leak(&quoted).is_ok());
        std::env::remove_var("NRG_SECRET_INTERP");
    }

    #[test]
    fn reveal_and_sh_quote_expose_plaintext_explicitly() {
        std::env::set_var("NRG_SECRET_DEMO2", "pa ss'wd");
        let e = secret_engine();
        let revealed: String = e.eval(r#"reveal(secret("DEMO2"))"#).unwrap();
        assert_eq!(revealed, "pa ss'wd");
        let quoted: String = e.eval(r#"sh_quote(secret("DEMO2"))"#).unwrap();
        assert_eq!(quoted, "'pa ss'\\''wd'");
        std::env::remove_var("NRG_SECRET_DEMO2");
    }

    #[test]
    fn secret_rejects_too_short() {
        std::env::set_var("NRG_SECRET_TINY", "ab");
        let e = secret_engine();
        assert!(e.eval::<rhai::Dynamic>(r#"secret("TINY")"#).is_err());
        std::env::remove_var("NRG_SECRET_TINY");
    }
}
