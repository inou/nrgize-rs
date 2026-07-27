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
///
/// `Err` means a file that DEFINES the requested key was refused as untrusted (see
/// `load_from_kv_file`); the caller must surface that rather than carrying on with some other
/// value — `Ok(None)` is the ordinary "not found anywhere" answer.
pub fn lookup_secret(
    root: Option<&std::path::Path>,
    name: &str,
    dest: Option<&str>,
) -> Result<Option<String>, String> {
    let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return Ok(Some(v));
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
        if let Some(v) = load_from_kv_file(&path, name)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
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

/// Read `key` out of a `KEY=VALUE` file, refusing the file if somebody other than the invoking
/// user could have written it (`crate::trust`) — a value read here becomes a password, and a
/// `CMD[...]`-framed one is handed to `sh -c` (`fetch_if_needed`), `--dry-run` included.
///
/// The check runs only once this file actually DEFINES the requested key, so a stray
/// foreign-owned file elsewhere in the search order (`.env` in a shared directory, say) changes
/// nothing about a secret it never mentions. It cannot be used to sneak a value past the check
/// either: adding the key to such a file is exactly what makes it refuse.
fn load_from_kv_file(path: &std::path::Path, key: &str) -> Result<Option<String>, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                if let Some(reason) = crate::trust::untrusted_reason(path) {
                    return Err(format!(
                        "refusing to read secret '{key}' from {}: {reason}. A secrets file must \
                         be owned by the user running nrg and must not be writable by other \
                         users — whoever can write it chooses the password nrg uses, and a \
                         CMD[...] value in it is run as a local shell command.",
                        path.display()
                    ));
                }
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(v);
                return Ok(Some(v.to_string()));
            }
        }
    }
    Ok(None)
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
                // A refused (untrusted) secrets file is an ERROR, not a miss: it must throw here
                // rather than fall through to a value from somewhere else, and the value it held
                // is never used.
                let found = lookup_secret(root.as_deref(), name, dest.as_deref())
                    .map_err(|e| -> Box<EvalAltResult> { e.into() })?;
                let raw = found.ok_or_else(
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
            lookup_secret(Some(tmp.path()), "DB_URL", Some("staging")).unwrap(),
            Some("staging-value".to_string())
        );
        // No --dest (or a dest whose file doesn't exist) falls through to the shared file.
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", None).unwrap(),
            Some("shared-value".to_string())
        );
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", Some("production")).unwrap(),
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
            lookup_secret(Some(tmp.path()), "SHARED_KEY", Some("staging")).unwrap(),
            Some("shared-value".to_string())
        );
    }

    #[cfg(unix)]
    fn chmod(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_refuses_a_world_writable_file_that_defines_the_key() {
        // Whoever can write the secrets file chooses the password — and a CMD[...] value in it is
        // run through `sh -c` (see `fetch_if_needed`), dry run included. A file other users can
        // write must be refused loudly, naming it and the reason, and its value never used.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        let secrets = tmp.path().join(".energize/secrets");
        std::fs::write(&secrets, "DB_PASSWORD=CMD[touch /tmp/pwned]\n").unwrap();
        chmod(&secrets, 0o666);

        let err = lookup_secret(Some(tmp.path()), "DB_PASSWORD", None)
            .expect_err("a world-writable secrets file must be refused, not read");
        assert!(err.contains(&secrets.display().to_string()), "must name the file: {err}");
        assert!(err.contains("writable by other users"), "must give the reason: {err}");
        assert!(!err.contains("touch /tmp/pwned"), "must not echo the planted value: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_ignores_an_untrusted_file_that_does_not_define_the_key() {
        // The check only fires for the file that actually DEFINES the requested key, so a stray
        // world-writable file elsewhere in the search order doesn't break secrets it never
        // mentions. (Adding the key TO that file is precisely what makes it refuse — see above.)
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        let loose = tmp.path().join(".energize/secrets");
        std::fs::write(&loose, "SOMETHING_ELSE=irrelevant\n").unwrap();
        chmod(&loose, 0o666);
        std::fs::write(tmp.path().join(".env"), "API_TOKEN=realsecretvalue\n").unwrap();

        assert_eq!(
            lookup_secret(Some(tmp.path()), "API_TOKEN", None).unwrap(),
            Some("realsecretvalue".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_accepts_a_group_writable_secrets_file() {
        // `0664` is the umask-002 default — an ordinary team checkout, not evidence of tampering.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        let secrets = tmp.path().join(".energize/secrets");
        std::fs::write(&secrets, "API_TOKEN=groupreadablevalue\n").unwrap();
        chmod(&secrets, 0o664);
        let env = tmp.path().join(".env");
        std::fs::write(&env, "OTHER_TOKEN=dotenvvalue\n").unwrap();
        chmod(&env, 0o664);

        assert_eq!(
            lookup_secret(Some(tmp.path()), "API_TOKEN", None).unwrap(),
            Some("groupreadablevalue".to_string())
        );
        assert_eq!(
            lookup_secret(Some(tmp.path()), "OTHER_TOKEN", None).unwrap(),
            Some("dotenvvalue".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_throws_and_runs_nothing_when_the_file_defining_it_is_untrusted() {
        // End-to-end through the `secret()` builtin: the refusal surfaces as a throw, and the
        // CMD[...] value in the refused file never reaches the runner.
        let _env_guard = crate::test_support::lock_env();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        let secrets = tmp.path().join(".energize/secrets");
        std::fs::write(&secrets, "DB_PASSWORD=CMD[echo attacker-controlled]\n").unwrap();
        chmod(&secrets, 0o666);

        let runner = std::sync::Arc::new(FakeRunner::new());
        let store = crate::engine::state::StateStore::load(tmp.path()).unwrap();
        let ctx = crate::engine::context::shared_with_state(
            runner.clone(),
            store,
            crate::engine::context::EffectMode::Live,
        );
        let mut e = Engine::new();
        register(&mut e, ctx);

        let err = e.eval::<rhai::Dynamic>(r#"secret("DB_PASSWORD")"#).unwrap_err();
        assert!(err.to_string().contains("writable by other users"), "got: {err}");
        assert!(runner.calls().is_empty(), "the CMD[...] value must never reach `sh -c`");
    }
}
