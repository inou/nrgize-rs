//! Tagged secret values + POSIX-safe quoting + trace redaction.

use crate::engine::context::SharedCtx;
use crate::engine::runner::CommandRunner;
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

/// Look up a secret by name: `NRG_SECRET_<UPPER>` env var, then (when `dest` is set)
/// `<root>/.energize/secrets.<dest>`, then `<root>/.energize/secrets`, then `<root>/.env` (all
/// `KEY=VALUE`, optional surrounding quotes). `root` is the discovered project root (the same
/// anchor as state); `None` falls back to CWD-relative paths (ephemeral/tests). Resolving
/// against `root` (not CWD) means running `nrg` from a subdirectory still finds the project's
/// secrets instead of silently missing them (issue #19).
///
/// The per-destination file (roadmap 2.2) is checked FIRST and does not replace the shared one —
/// a key present only in the shared `.energize/secrets` still resolves for every destination,
/// so a team doesn't need to duplicate every secret into each destination's file, only the ones
/// that actually differ (e.g. a per-environment database URL).
pub fn lookup_secret(root: Option<&std::path::Path>, name: &str, dest: Option<&str>) -> Option<String> {
    let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return Some(v);
    }
    let dest_rel = dest.map(|d| format!(".energize/secrets.{d}"));
    let rels: Vec<&str> = dest_rel
        .as_deref()
        .into_iter()
        .chain([".energize/secrets", ".env"])
        .collect();
    for rel in rels {
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

/// If `raw` is an `ENC[...]` token (the framing `nrg secrets encrypt`/`seal` produce), decrypt
/// it via the discovered `.nrg-key` before it's ever used as a secret value. A plain value
/// passes through unchanged — this only engages for the documented ENC[...]-in-config workflow.
/// Length checking (`MIN_SECRET_LEN`) happens on the RESULT of this, not the raw ciphertext,
/// which is always long regardless of the underlying secret's real length.
fn decrypt_if_needed(raw: &str, name: &str) -> Result<String, Box<EvalAltResult>> {
    if !(raw.starts_with("ENC[") && raw.ends_with(']')) {
        return Ok(raw.to_string());
    }
    let key_path = crate::secrets::find_key_file().ok_or_else(|| -> Box<EvalAltResult> {
        format!(
            "secret '{name}' is an encrypted ENC[...] token but no .nrg-key was found to decrypt \
             it (run `nrg secrets init`, or provide a plain value via $NRG_SECRET_{} instead)",
            name.to_uppercase()
        )
        .into()
    })?;
    crate::secrets::decrypt_value(raw, &key_path)
        .map_err(|e| -> Box<EvalAltResult> { format!("secret '{name}': failed to decrypt: {e}").into() })
}

/// If `raw` is a `CMD[...]` token — the fetch-adapter convention (roadmap 2.4 step 2): a value
/// framed this way is a shell command, run locally, whose (trailing-newline-trimmed) stdout IS
/// the secret. Kamal-style: `1Password`/Bitwarden/Vault/Doppler all reduce to "run some CLI,
/// capture its stdout" (`op read op://vault/item/field`, `vault kv get -field=x ...`, etc.), so
/// rather than inventing a config schema for each backend, `nrg` just runs whatever command the
/// user writes — the same "arbitrary shell snippet, evaluated once" shape Kamal's own
/// `.kamal/secrets` convention uses (there `$(op read ...)` inside a `KEY=VALUE` line). `CMD[...]`
/// (mirroring the existing `ENC[...]` bracket framing already established by this file, rather
/// than shell `$(...)` syntax) avoids ambiguity with a legitimate value that happens to contain a
/// literal `$(...)` substring, and keeps every "special" value in this file recognizable by the
/// same bracket-prefix convention. A plain (non-`CMD[`) value passes through unchanged — this
/// only engages for the documented `CMD[...]`-in-config workflow, same as `decrypt_if_needed`'s
/// `ENC[...]` check. Runs regardless of `--dry-run`: a script needs the real value to plan
/// against (e.g. `sh_quote(secret(...))` embedded in a command being rendered), exactly like
/// `ENC[...]` decryption already runs unconditionally.
fn fetch_if_needed(raw: &str, name: &str, runner: &dyn CommandRunner) -> Result<String, Box<EvalAltResult>> {
    if !(raw.starts_with("CMD[") && raw.ends_with(']')) {
        return Ok(raw.to_string());
    }
    let cmd = &raw[4..raw.len() - 1];
    if cmd.trim().is_empty() {
        return Err(format!("secret '{name}' is CMD[...]-framed but the command is empty").into());
    }
    let out = runner.run_local(cmd);
    if out.exit_code != 0 {
        return Err(format!(
            "secret '{name}': fetch command failed (exit {}): {}",
            out.exit_code,
            out.stderr.trim()
        )
        .into());
    }
    Ok(out.stdout.trim_end_matches(['\r', '\n']).to_string())
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

    // secret(name) -> Secret  (throws if missing, too short, or an ENC[...] token that fails to decrypt)
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "secret",
            move |name: &str| -> Result<Secret, Box<EvalAltResult>> {
                // Resolve secret files against the project root (same anchor as state), not CWD,
                // and against the active destination (roadmap 2.2) so `--dest staging` checks
                // `.energize/secrets.staging` before the shared `.energize/secrets`.
                let (root, dest) = {
                    let store = ctx.state.lock().unwrap();
                    (store.root(), store.dest())
                };
                let raw = lookup_secret(root.as_deref(), name, dest.as_deref()).ok_or_else(
                    || -> Box<EvalAltResult> {
                        let dest_hint = match &dest {
                            Some(d) => format!(".energize/secrets.{d}, "),
                            None => String::new(),
                        };
                        format!(
                            "secret '{name}' not found (checked $NRG_SECRET_{}, {dest_hint}\
                             .energize/secrets, .env)",
                            name.to_uppercase()
                        )
                        .into()
                    },
                )?;
                // A CMD[...]-framed value (roadmap 2.4 step 2) is a fetch-adapter command, run
                // locally, whose stdout becomes the raw value — BEFORE the ENC[...] check, so a
                // fetched value could itself (unusually, but harmlessly) also be an ENC[...]
                // token. `ctx.runner` is the real (non-dry-run) runner regardless of `ctx.mode` —
                // see this function's doc comment for why that's correct here.
                let raw = fetch_if_needed(&raw, name, ctx.runner.as_ref())?;
                // `nrg secrets encrypt` produces an ENC[...] token meant to be pasted into config
                // or .env; decrypt it transparently HERE so that documented workflow actually
                // works, instead of the raw ciphertext silently becoming the "secret" value.
                let value = decrypt_if_needed(&raw, name)?;
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
    fn decrypt_if_needed_passes_a_plain_value_through_unchanged() {
        // No ENC[...] framing — must not touch the filesystem looking for a key at all.
        assert_eq!(decrypt_if_needed("plain-value", "X").unwrap(), "plain-value");
    }

    #[test]
    fn decrypt_if_needed_requires_both_the_prefix_and_suffix() {
        // A value that merely starts with "ENC[" (e.g. truncated, or a coincidental prefix)
        // without the closing bracket is NOT ENC[...]-framed — pass it through, don't error.
        assert_eq!(decrypt_if_needed("ENC[incomplete", "X").unwrap(), "ENC[incomplete");
    }

    #[test]
    fn fetch_if_needed_passes_a_plain_value_through_unchanged() {
        // No CMD[...] framing — must not touch the runner at all.
        let runner = FakeRunner::new();
        assert_eq!(fetch_if_needed("plain-value", "X", &runner).unwrap(), "plain-value");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn fetch_if_needed_requires_both_the_prefix_and_suffix() {
        // Same "must be fully framed" rule as decrypt_if_needed's ENC[...] check.
        let runner = FakeRunner::new();
        assert_eq!(fetch_if_needed("CMD[incomplete", "X", &runner).unwrap(), "CMD[incomplete");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn fetch_if_needed_rejects_an_empty_command() {
        let runner = FakeRunner::new();
        let err = fetch_if_needed("CMD[]", "X", &runner).unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
        assert!(runner.calls().is_empty(), "must not shell out to an empty command");
    }

    #[test]
    fn fetch_if_needed_runs_the_command_and_trims_trailing_newlines() {
        let mut runner = FakeRunner::new();
        runner.default =
            crate::engine::runner::RawOutput { stdout: "fetched-value\n".to_string(), stderr: String::new(), exit_code: 0 };
        let value = fetch_if_needed("CMD[op read op://vault/item/field]", "X", &runner).unwrap();
        assert_eq!(value, "fetched-value");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("op read op://vault/item/field"), "got: {calls:?}");
    }

    #[test]
    fn fetch_if_needed_trims_a_trailing_crlf_too() {
        // Fable review: a fetch CLI that emits CRLF (uncommon on the documented Unix
        // 1Password/Bitwarden/Vault/Doppler tools, but possible) must not leave a trailing \r
        // in the secret — that would silently break a downstream use like a docker login.
        let mut runner = FakeRunner::new();
        runner.default =
            crate::engine::runner::RawOutput { stdout: "fetched-value\r\n".to_string(), stderr: String::new(), exit_code: 0 };
        let value = fetch_if_needed("CMD[op read op://vault/item/field]", "X", &runner).unwrap();
        assert_eq!(value, "fetched-value");
    }

    #[test]
    fn fetch_if_needed_surfaces_a_command_failure_with_its_stderr() {
        let mut runner = FakeRunner::new();
        runner.default = crate::engine::runner::RawOutput {
            stdout: String::new(),
            stderr: "1Password: not signed in".to_string(),
            exit_code: 1,
        };
        let err = fetch_if_needed("CMD[op read op://vault/item/field]", "X", &runner).unwrap_err();
        assert!(err.to_string().contains("not signed in"), "got: {err}");
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

    /// Same as `secret_engine`, but over a caller-configured `FakeRunner` — needed for the
    /// CMD[...] fetch-adapter tests, which (unlike every other test in this module) need
    /// `secret()`'s underlying runner to actually return a specific canned command output.
    fn secret_engine_with_runner(runner: std::sync::Arc<FakeRunner>) -> Engine {
        let ctx = shared(runner);
        let mut e = Engine::new();
        register(&mut e, ctx);
        e
    }

    #[test]
    fn secret_to_string_yields_sentinel_and_no_string_coercion() {
        // Robustness review: "Flaky patterns" — serialize against every other env-mutating test
        // in this binary (parallel test threads + set_var/getenv racing is UB-adjacent on glibc).
        let _env_guard = crate::test_support::lock_env();
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
        let _env_guard = crate::test_support::lock_env();
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
        let _env_guard = crate::test_support::lock_env();
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
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_TINY", "ab");
        let e = secret_engine();
        assert!(e.eval::<rhai::Dynamic>(r#"secret("TINY")"#).is_err());
        std::env::remove_var("NRG_SECRET_TINY");
    }

    #[test]
    fn secret_resolves_a_cmd_framed_value_via_the_fetch_command_end_to_end() {
        // Roadmap 2.4 step 2: a CMD[...]-framed value (from ANY tier — here the env var one, the
        // simplest to set up) runs the command via the engine's real runner and uses its trimmed
        // stdout as the secret, going through the exact same redaction/Secret-wrapping pipeline
        // a file/env-sourced value already does.
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_OP_TOKEN", "CMD[op read op://vault/item/field]");
        let mut runner = FakeRunner::new();
        runner.default = crate::engine::runner::RawOutput {
            stdout: "fetched-secret-value\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
        };
        let e = secret_engine_with_runner(std::sync::Arc::new(runner));
        let revealed: String = e.eval(r#"reveal(secret("OP_TOKEN"))"#).unwrap();
        assert_eq!(revealed, "fetched-secret-value");
        // The fetched value gets registered for redaction exactly like any other secret.
        let s: String = e.eval(r#"secret("OP_TOKEN").to_string()"#).unwrap();
        assert_eq!(s, SECRET_SENTINEL);
        std::env::remove_var("NRG_SECRET_OP_TOKEN");
    }

    #[test]
    fn secret_surfaces_a_fetch_command_failure_end_to_end() {
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_OP_TOKEN2", "CMD[op read op://vault/item/field]");
        let mut runner = FakeRunner::new();
        runner.default = crate::engine::runner::RawOutput {
            stdout: String::new(),
            stderr: "1Password: not signed in".to_string(),
            exit_code: 1,
        };
        let e = secret_engine_with_runner(std::sync::Arc::new(runner));
        let err = e.eval::<rhai::Dynamic>(r#"secret("OP_TOKEN2")"#).unwrap_err();
        assert!(err.to_string().contains("not signed in"), "got: {err}");
        std::env::remove_var("NRG_SECRET_OP_TOKEN2");
    }

    #[test]
    fn lookup_secret_prefers_the_destination_file_over_the_shared_one() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        std::fs::write(tmp.path().join(".energize/secrets"), "DB_URL=shared-value\n").unwrap();
        std::fs::write(tmp.path().join(".energize/secrets.staging"), "DB_URL=staging-value\n").unwrap();
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", Some("staging")),
            Some("staging-value".to_string())
        );
        // No --dest (or a dest whose file doesn't exist) falls through to the shared file.
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", None),
            Some("shared-value".to_string())
        );
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", Some("production")),
            Some("shared-value".to_string())
        );
    }

    #[test]
    fn lookup_secret_falls_back_to_the_shared_file_for_a_key_absent_from_the_destination_file() {
        // A destination file need only override the keys that actually DIFFER per environment —
        // any key it doesn't mention still resolves from the shared `.energize/secrets`.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        std::fs::write(tmp.path().join(".energize/secrets"), "SHARED_KEY=shared-value\n").unwrap();
        std::fs::write(tmp.path().join(".energize/secrets.staging"), "DB_URL=staging-value\n").unwrap();
        assert_eq!(
            lookup_secret(Some(tmp.path()), "SHARED_KEY", Some("staging")),
            Some("shared-value".to_string())
        );
    }
}
