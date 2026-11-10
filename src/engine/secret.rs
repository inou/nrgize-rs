//! Tagged secret values + POSIX-safe quoting + trace redaction.

use crate::engine::context::SharedCtx;
use rhai::{Engine, EvalAltResult};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Where a secret's value came from. An environment value is already plaintext; a file value is
/// the raw `ENC[...]` token, still to be decrypted.
#[derive(Debug, PartialEq)]
pub enum Resolved {
    Env(String),
    File { token: String, path: PathBuf },
}

/// Look up a secret by name, first hit wins: the `NRG_SECRET_<UPPER>` env var (plaintext, for
/// CI), then (when `dest` is set) `<root>/.energize/secrets.<dest>`, then
/// `<root>/.energize/secrets` (`KEY=ENC[...]` lines, optional surrounding quotes). `root` is the
/// discovered project root (the same anchor as state); `None` falls back to CWD-relative paths
/// (ephemeral/tests). Resolving against `root` (not CWD) means running `nrg` from a subdirectory
/// still finds the project's secrets instead of silently missing them (issue #19).
///
/// The per-destination file is checked FIRST and does not replace the shared one — a key present
/// only in the shared `.energize/secrets` still resolves for every destination, so a team only
/// needs to repeat the secrets that actually differ (e.g. a per-environment database URL).
///
/// `Err` means a source that DEFINES the requested key was refused (untrusted file, plaintext in
/// a secrets file, `ENC[...]` in the environment); the caller must surface that rather than
/// carrying on with some other value — `Ok(None)` is the ordinary "not found anywhere" answer.
pub fn lookup_secret(
    root: Option<&Path>,
    name: &str,
    dest: Option<&str>,
) -> Result<Option<Resolved>, String> {
    let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return check_env_value(name, v).map(|v| Some(Resolved::Env(v)));
    }
    let dest_rel = dest.map(|d| format!(".energize/secrets.{d}"));
    let rels: Vec<&str> = dest_rel
        .as_deref()
        .into_iter()
        .chain([".energize/secrets"])
        .collect();
    for rel in rels {
        let path = match root {
            Some(r) => r.join(rel),
            None => PathBuf::from(rel),
        };
        if let Some(token) = load_from_kv_file(&path, name)? {
            if !is_enc_framed(&token) {
                let dest_flag = if rel == ".energize/secrets" {
                    String::new()
                } else {
                    format!(" --dest {}", dest.unwrap_or_default())
                };
                return Err(format!(
                    "secret '{name}' in {} is not encrypted; plaintext values are not allowed in \
                     secrets files. Replace it with `nrg secrets encrypt {name}{dest_flag}`, or \
                     supply it via $NRG_SECRET_{}.",
                    path.display(),
                    name.to_uppercase()
                ));
            }
            return Ok(Some(Resolved::File { token, path }));
        }
    }
    Ok(None)
}

fn is_enc_framed(v: &str) -> bool {
    v.starts_with("ENC[") && v.ends_with(']')
}

/// The environment variable carries the plaintext. An `ENC[...]` token there is refused rather
/// than used literally, which would silently make the ciphertext the password.
fn check_env_value(name: &str, v: String) -> Result<String, String> {
    if is_enc_framed(&v) {
        return Err(format!(
            "secret '{name}': $NRG_SECRET_{} holds an ENC[...] token; the environment variable \
             must hold the plaintext (encrypted values belong in .energize/secrets)",
            name.to_uppercase()
        ));
    }
    Ok(v)
}

/// Decrypt an `ENC[...]` token from a secrets file with the discovered `.nrg-key`. Length
/// checking (`MIN_SECRET_LEN`) happens on the RESULT, not the ciphertext, which is always long
/// regardless of the underlying secret's real length.
fn decrypt_token(token: &str, name: &str, path: &Path) -> Result<String, Box<EvalAltResult>> {
    let key_path = crate::secrets::find_key_file().ok_or_else(|| -> Box<EvalAltResult> {
        format!(
            "secret '{name}' in {} is encrypted but no .nrg-key was found to decrypt it (run \
             `nrg secrets init` for a new project or copy the team's .nrg-key here; in CI set \
             $NRG_SECRET_{} instead)",
            path.display(),
            name.to_uppercase()
        )
        .into()
    })?;
    crate::secrets::decrypt_value(token, &key_path).map_err(|e| -> Box<EvalAltResult> {
        format!("secret '{name}': failed to decrypt: {e}").into()
    })
}

/// Read `key` out of a `KEY=VALUE` file, refusing the file if somebody other than the invoking
/// user could have written it (`crate::trust`) — whoever can write it can substitute a
/// ciphertext and so choose the password nrg uses.
///
/// The check runs only once this file actually DEFINES the requested key, so a stray
/// foreign-owned file elsewhere in the search order changes nothing about a secret it never
/// mentions. It cannot be used to sneak a value past the check either: adding the key to such a
/// file is exactly what makes it refuse.
fn load_from_kv_file(path: &Path, key: &str) -> Result<Option<String>, String> {
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
                         users — whoever can write it can substitute a ciphertext and so choose \
                         the password nrg uses.",
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
    // Explicit env/stdin values may be shorter than secret()'s minimum. Every registered
    // value must be protected in plans and print() as well as streamed output.
    let values: Vec<_> = secrets.iter().cloned().collect();
    String::from_utf8_lossy(
        &crate::engine::diagnostics::Redactor::new(&values).push(text.as_bytes(), true),
    )
    .into_owned()
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

    // secret(name) -> Secret  (throws if missing, refused, too short, or an ENC[...] token that fails to decrypt)
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "secret",
            move |name: &str| -> Result<Secret, Box<EvalAltResult>> {
                // Resolve secret files against the project root (same anchor as state), not CWD,
                // and against the active destination so `--dest staging` checks
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
                let found = found.ok_or_else(|| -> Box<EvalAltResult> {
                    let dest_hint = match &dest {
                        Some(d) => format!(".energize/secrets.{d}, "),
                        None => String::new(),
                    };
                    format!(
                        "secret '{name}' not found (checked $NRG_SECRET_{}, {dest_hint}\
                         .energize/secrets)",
                        name.to_uppercase()
                    )
                    .into()
                })?;
                let value = match found {
                    Resolved::Env(v) => v,
                    Resolved::File { token, path } => decrypt_token(&token, name, &path)?,
                };
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
    engine.register_fn("sh_quote", |s: Secret| -> String {
        posix_quote(s.reveal())
    });

    // Forbid string concatenation with a Secret. Rhai would otherwise auto-stringify it via
    // to_string() (= "***"), silently producing a broken `... + ***` command. Failing loudly
    // forces sh_quote(secret) for shell args or reveal(secret) for explicit plaintext.
    engine.register_fn(
        "+",
        |_a: &str, _b: Secret| -> Result<String, Box<EvalAltResult>> { Err(NO_CONCAT.into()) },
    );
    engine.register_fn(
        "+",
        |_a: Secret, _b: &str| -> Result<String, Box<EvalAltResult>> { Err(NO_CONCAT.into()) },
    );
    engine.register_fn(
        "+",
        |_a: Secret, _b: Secret| -> Result<String, Box<EvalAltResult>> { Err(NO_CONCAT.into()) },
    );
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
        s.insert("ab".to_string()); // explicit step input is protected even when short
        let out = redact("token=supersecretvalue and ab and abacus", &s);
        assert_eq!(out, "token=*** and *** and ***acus");
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
        // Serialize against every other env-mutating test
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
        assert!(
            cmd.contains(SECRET_SENTINEL),
            "interpolation must yield the sentinel"
        );
        assert!(
            !cmd.contains("hunter2value"),
            "interpolation must not leak plaintext"
        );
        assert!(
            assert_no_secret_leak(&cmd).is_err(),
            "boundary guard must reject the sentinel"
        );
        // The supported paths still work and pass the guard.
        let quoted: String = e
            .eval(r#"`docker login -p ${sh_quote(secret("INTERP"))}`"#)
            .unwrap();
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

    fn file_secrets(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir.join(".energize")).unwrap();
        let p = dir.join(".energize").join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn file(token: &str, path: PathBuf) -> Option<Resolved> {
        Some(Resolved::File {
            token: token.to_string(),
            path,
        })
    }

    #[test]
    fn lookup_secret_prefers_the_destination_file_over_the_shared_one() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = file_secrets(tmp.path(), "secrets", "DB_URL=ENC[shared]\n");
        let staging = file_secrets(tmp.path(), "secrets.staging", "DB_URL=ENC[staging]\n");
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", Some("staging")).unwrap(),
            file("ENC[staging]", staging)
        );
        // No --dest (or a dest whose file doesn't exist) falls through to the shared file.
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", None).unwrap(),
            file("ENC[shared]", shared.clone())
        );
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DB_URL", Some("production")).unwrap(),
            file("ENC[shared]", shared)
        );
    }

    #[test]
    fn lookup_secret_falls_back_to_the_shared_file_for_a_key_absent_from_the_destination_file() {
        // A destination file need only override the keys that actually DIFFER per environment —
        // any key it doesn't mention still resolves from the shared `.energize/secrets`.
        let tmp = tempfile::tempdir().unwrap();
        let shared = file_secrets(tmp.path(), "secrets", "SHARED_KEY=ENC[shared]\n");
        file_secrets(tmp.path(), "secrets.staging", "DB_URL=ENC[staging]\n");
        assert_eq!(
            lookup_secret(Some(tmp.path()), "SHARED_KEY", Some("staging")).unwrap(),
            file("ENC[shared]", shared)
        );
    }

    #[test]
    fn lookup_secret_strips_quotes_around_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let p = file_secrets(tmp.path(), "secrets", "A=\"ENC[one]\"\nB='ENC[two]'\n");
        assert_eq!(
            lookup_secret(Some(tmp.path()), "A", None).unwrap(),
            file("ENC[one]", p.clone())
        );
        assert_eq!(
            lookup_secret(Some(tmp.path()), "B", None).unwrap(),
            file("ENC[two]", p)
        );
    }

    #[test]
    fn lookup_secret_refuses_plaintext_without_echoing_it() {
        let tmp = tempfile::tempdir().unwrap();
        file_secrets(tmp.path(), "secrets", "K=hunter2-plain\n");
        let err = lookup_secret(Some(tmp.path()), "K", None).unwrap_err();
        assert!(err.contains("nrg secrets encrypt K"), "{err}");
        assert!(!err.contains("--dest"), "{err}");
        assert!(!err.contains("hunter2-plain"), "{err}");
    }

    #[test]
    fn an_unterminated_enc_token_in_a_file_is_refused_as_plaintext() {
        let tmp = tempfile::tempdir().unwrap();
        file_secrets(tmp.path(), "secrets", "K=ENC[abc\n");
        let err = lookup_secret(Some(tmp.path()), "K", None).unwrap_err();
        assert!(err.contains("is not encrypted"), "{err}");
        assert!(!err.contains("ENC[abc"), "{err}");
    }

    #[test]
    fn plaintext_in_the_dest_file_refusal_names_dest() {
        let tmp = tempfile::tempdir().unwrap();
        file_secrets(tmp.path(), "secrets.staging", "K=plainvalue\n");
        let err = lookup_secret(Some(tmp.path()), "K", Some("staging")).unwrap_err();
        assert!(
            err.contains("nrg secrets encrypt K --dest staging"),
            "{err}"
        );
        // Not a miss: a bad dest file must not fall through to the shared file.
        file_secrets(tmp.path(), "secrets", "K=ENC[shared]\n");
        assert!(lookup_secret(Some(tmp.path()), "K", Some("staging")).is_err());
    }

    #[test]
    fn lookup_secret_ignores_dotenv() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "DOTENV_ONLY=realsecretvalue\n").unwrap();
        assert_eq!(
            lookup_secret(Some(tmp.path()), "DOTENV_ONLY", None).unwrap(),
            None
        );
    }

    #[test]
    fn check_env_value_refuses_enc_and_passes_plaintext() {
        assert_eq!(
            check_env_value("K", "plain-value".into()).unwrap(),
            "plain-value"
        );
        assert_eq!(
            check_env_value("K", "ENC[incomplete".into()).unwrap(),
            "ENC[incomplete"
        );
        let err = check_env_value("K", "ENC[abc]".into()).unwrap_err();
        assert!(
            err.contains("$NRG_SECRET_K holds an ENC[...] token"),
            "{err}"
        );
        assert!(!err.contains("ENC[abc]"), "{err}");
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_refuses_a_world_writable_file_that_defines_the_key() {
        // Whoever can write the secrets file can substitute a ciphertext and so choose the
        // password. A file other users can write must be refused loudly, naming it and the
        // reason, and its value never used.
        let tmp = tempfile::tempdir().unwrap();
        let secrets = file_secrets(tmp.path(), "secrets", "DB_PASSWORD=ENC[planted-marker]\n");
        chmod(&secrets, 0o666);

        let err = lookup_secret(Some(tmp.path()), "DB_PASSWORD", None)
            .expect_err("a world-writable secrets file must be refused, not read");
        assert!(
            err.contains(&secrets.display().to_string()),
            "must name the file: {err}"
        );
        assert!(
            err.contains("writable by other users"),
            "must give the reason: {err}"
        );
        assert!(
            !err.contains("planted-marker"),
            "must not echo the planted value: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_ignores_an_untrusted_file_that_does_not_define_the_key() {
        // The check only fires for the file that actually DEFINES the requested key, so a stray
        // world-writable file elsewhere in the search order doesn't break secrets it never
        // mentions. (Adding the key TO that file is precisely what makes it refuse — see above.)
        let tmp = tempfile::tempdir().unwrap();
        // The destination file is searched first and is the untrusted one; it lacks the key, so
        // the lookup must read it, skip it and answer from the shared file.
        let staging = file_secrets(
            tmp.path(),
            "secrets.staging",
            "SOMETHING_ELSE=ENC[irrelevant]\n",
        );
        chmod(&staging, 0o666);
        let shared = file_secrets(tmp.path(), "secrets", "API_TOKEN=ENC[real]\n");

        assert_eq!(
            lookup_secret(Some(tmp.path()), "API_TOKEN", Some("staging")).unwrap(),
            file("ENC[real]", shared)
        );
    }

    #[cfg(unix)]
    #[test]
    fn lookup_secret_accepts_a_group_writable_secrets_file() {
        // `0664` is the umask-002 default — an ordinary team checkout, not evidence of tampering.
        let tmp = tempfile::tempdir().unwrap();
        let secrets = file_secrets(tmp.path(), "secrets", "API_TOKEN=ENC[grouped]\n");
        chmod(&secrets, 0o664);

        assert_eq!(
            lookup_secret(Some(tmp.path()), "API_TOKEN", None).unwrap(),
            file("ENC[grouped]", secrets)
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_throws_when_the_file_defining_it_is_untrusted() {
        // End-to-end through the `secret()` builtin: the refusal surfaces as a throw.
        let _env_guard = crate::test_support::lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let secrets = file_secrets(tmp.path(), "secrets", "DB_PASSWORD=ENC[attacker]\n");
        chmod(&secrets, 0o666);

        let store = crate::engine::state::StateStore::load(tmp.path()).unwrap();
        let ctx = crate::engine::context::shared_with_state(
            FakeRunner::shared(),
            store,
            crate::engine::context::EffectMode::Live,
        );
        let mut e = Engine::new();
        register(&mut e, ctx);

        let err = e
            .eval::<rhai::Dynamic>(r#"secret("DB_PASSWORD")"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("writable by other users"),
            "got: {err}"
        );
    }
}
