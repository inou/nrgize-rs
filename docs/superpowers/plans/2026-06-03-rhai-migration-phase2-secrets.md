# Rhai Migration — Phase 2: Secrets — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A tagged `Secret` type so secret values can't accidentally leak (no `String`
coercion → `print`, concat, and `state_set` all fail loudly), retrieved via `secret(name)`,
used only through explicit `reveal()`/`sh_quote()`, with any registered secret value redacted
from `NRG_TRACE` output as defense-in-depth.

**Architecture:** New `src/engine/secret.rs` owns the `Secret(String)` newtype, the POSIX
`sh_quote`, secret lookup (`NRG_SECRET_<UPPER>` env → `.energize/secrets` → `.env`), and a
`redact()` helper. `RunCtx` gains `secrets: Arc<Mutex<HashSet<String>>>`; `secret()` registers
each resolved value there. The exec builtins redact `cmd` against that set before tracing.

**Architecture note — two §9 items are already obviated:** "stop exporting secrets as remote
env vars" and "gate the `DEBUG` trap on `NRG_TRACE`" applied to the *legacy* `build_script`
heredoc (the `run` path). The new engine's `ssh_exec(host, cmd)` runs one command via argv —
**no auto env-export, no `DEBUG` trap** — so those leak vectors don't exist here and vanish
entirely when `build_script` is deleted in P6.

**Tech Stack:** rhai custom type + overloaded `register_fn`; `std::collections::HashSet`.

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/secret.rs` | `Secret` newtype, `posix_quote`/`sh_quote`, `lookup_secret`, `redact`, `MIN_SECRET_LEN`, builtin registration. |
| `src/engine/context.rs` | Add `secrets: Arc<Mutex<HashSet<String>>>` + `register_secret`. |
| `src/engine/builtins/exec.rs` | Redact `cmd` against the secret set in trace output. |
| `src/engine/mod.rs` | `pub mod secret;` + register secret builtins in `build_engine`. |

---

## Task 1: `Secret` type, `sh_quote`, lookup, `redact` (pure core)

**Files:** Create `src/engine/secret.rs`; Modify `src/engine/mod.rs` (`pub mod secret;`)

- [ ] **Step 1: Declare module** — add `pub mod secret;` to `src/engine/mod.rs`.

- [ ] **Step 2: Write the core + tests**

Create `src/engine/secret.rs`:

```rust
//! Tagged secret values + POSIX-safe quoting + trace redaction.

use std::collections::HashSet;

/// Secrets shorter than this are rejected: substring redaction can't safely distinguish a
/// very short secret from ordinary output (and a too-short secret is weak anyway).
pub const MIN_SECRET_LEN: usize = 6;

/// A secret value. Deliberately NOT convertible to `String` in scripts: the only ways to get
/// the plaintext are `reveal()` / `sh_quote()`, so every plaintext use is explicit.
#[derive(Debug, Clone)]
pub struct Secret(String);

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

/// Look up a secret by name: `NRG_SECRET_<UPPER>` env var, then `.energize/secrets`, then
/// `.env` (both `KEY=VALUE`, optional surrounding quotes). Returns the raw plaintext.
pub fn lookup_secret(name: &str) -> Option<String> {
    let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return Some(v);
    }
    for file in [".energize/secrets", ".env"] {
        if let Some(v) = load_from_kv_file(file, name) {
            return Some(v);
        }
    }
    None
}

fn load_from_kv_file(path: &str, key: &str) -> Option<String> {
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

/// Replace every registered secret value in `text` with `***`. Defense-in-depth for trace
/// output; the primary protection is the `Secret` type itself.
pub fn redact(text: &str, secrets: &HashSet<String>) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.len() >= MIN_SECRET_LEN {
            out = out.replace(s.as_str(), "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 3: Run** `cargo test --bin nrg engine::secret` → PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/mod.rs src/engine/secret.rs
git commit -m "feat(secret): Secret newtype + POSIX sh_quote + lookup + redact (pure core)"
```

---

## Task 2: RunCtx carries the secret set

**Files:** Modify `src/engine/context.rs`

- [ ] **Step 1: Add the field + helper**

In `src/engine/context.rs`: add imports and the field.

```rust
use std::collections::HashSet;
```

Add to `struct RunCtx` (after `state`):

```rust
    /// Plaintext values of resolved secrets, for trace/plan redaction.
    pub secrets: Arc<Mutex<HashSet<String>>>,
```

In `RunCtx::build`, initialize it:

```rust
            secrets: Arc::new(Mutex::new(HashSet::new())),
```

Add a convenience method on `impl RunCtx`:

```rust
    /// Register a resolved secret value for redaction.
    pub fn register_secret(&self, value: &str) {
        self.secrets.lock().unwrap().insert(value.to_string());
    }
```

- [ ] **Step 2: Run** `cargo test --bin nrg engine::context` → PASS (existing test still green).

- [ ] **Step 3: Commit**

```bash
git add src/engine/context.rs
git commit -m "feat(secret): RunCtx carries the secret set for redaction"
```

---

## Task 3: `secret` / `reveal` / `sh_quote` builtins

**Files:** Modify `src/engine/secret.rs` (add `register`); Modify `src/engine/mod.rs` (call it in `build_engine`)

- [ ] **Step 1: Add `register` to `src/engine/secret.rs`** (after `redact`, before tests):

```rust
use crate::engine::context::SharedCtx;
use rhai::{Engine, EvalAltResult};
use std::sync::Arc;

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    engine
        .register_type_with_name::<Secret>("Secret")
        .register_fn("to_string", |_s: &mut Secret| "***".to_string())
        .register_fn("to_debug", |_s: &mut Secret| "***".to_string());

    // secret(name) -> Secret  (throws if missing or too short)
    {
        let ctx = ctx.clone();
        engine.register_fn("secret", move |name: &str| -> Result<Secret, Box<EvalAltResult>> {
            let value = lookup_secret(name).ok_or_else(|| -> Box<EvalAltResult> {
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
            // Snapshot the secret set out of the RunCtx lock, then register for redaction.
            let secrets: Arc<_> = ctx.lock().unwrap().secrets.clone();
            secrets.lock().unwrap().insert(value.clone());
            Ok(Secret::new(value))
        });
    }

    // reveal(secret) -> String   (explicit un-wrap)
    engine.register_fn("reveal", |s: Secret| -> String { s.reveal().to_string() });

    // sh_quote(x) -> String   for both String and Secret (the only safe interpolation path)
    engine.register_fn("sh_quote", |s: &str| -> String { posix_quote(s) });
    engine.register_fn("sh_quote", |s: Secret| -> String { posix_quote(s.reveal()) });
}
```

- [ ] **Step 2: Call it in `build_engine`** — in `src/engine/mod.rs`, after `builtins::register_builtins(...)`:

```rust
    secret::register(&mut engine, ctx);
```

> Note: `register_builtins` takes `ctx` by value (last `.clone()` is dropped). Reorder so both get a clone: change the `register_builtins` call to `builtins::register_builtins(&mut engine, ctx.clone());` then `secret::register(&mut engine, ctx);`.

- [ ] **Step 3: Add builtin-level tests** to `src/engine/secret.rs` tests module:

```rust
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    fn secret_engine() -> Engine {
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        register(&mut e, ctx);
        e
    }

    #[test]
    fn secret_to_string_is_redacted_and_no_string_coercion() {
        std::env::set_var("NRG_SECRET_DEMO", "topsecretvalue");
        let e = secret_engine();
        // to_string() / print path shows ***
        let shown: String = e.eval(r#"secret("DEMO").to_string()"#).unwrap();
        assert_eq!(shown, "***");
        // string concat with a Secret must NOT compile/eval (no `+` operator registered).
        assert!(e.eval::<String>(r#""x" + secret("DEMO")"#).is_err());
        std::env::remove_var("NRG_SECRET_DEMO");
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
```

- [ ] **Step 4: Run** `cargo test --bin nrg engine::secret` → PASS (6 tests).

> If `to_string` registration doesn't override `print` output, that's still safe (it would
> show the type name, never plaintext) — but the test asserts `***`; if it fails, switch to
> asserting the output does NOT contain the plaintext.

- [ ] **Step 5: Commit**

```bash
git add src/engine/secret.rs src/engine/mod.rs
git commit -m "feat(secret): secret()/reveal()/sh_quote() builtins + redacted Display"
```

---

## Task 4: Redact secrets in exec trace output

**Files:** Modify `src/engine/builtins/exec.rs`

- [ ] **Step 1: Thread the secret set into the snapshot + redact trace**

In `src/engine/builtins/exec.rs`, change `snapshot` to also return the secret set, and redact
the traced command. Replace the `snapshot` fn:

```rust
use std::collections::HashSet;

/// Snapshot (mode, runner, trace, secrets) under a short lock, then release before blocking.
fn snapshot(ctx: &SharedCtx) -> (EffectMode, Arc<dyn CommandRunner>, bool, Arc<std::sync::Mutex<HashSet<String>>>) {
    let g = ctx.lock().unwrap();
    (g.mode, g.runner.clone(), g.trace, g.secrets.clone())
}

/// Redact a command for trace display against the registered secret values.
fn traced(cmd: &str, secrets: &Arc<std::sync::Mutex<HashSet<String>>>) -> String {
    crate::engine::secret::redact(cmd, &secrets.lock().unwrap())
}
```

Then in each builtin, update the destructuring and the trace line. For `ssh_exec`:

```rust
        engine.register_fn("ssh_exec", move |host: &str, cmd: &str| -> ExecResult {
            let (mode, runner, trace, secrets) = snapshot(&ctx);
            if trace {
                eprintln!("[nrg] ssh_exec {host} -> {}", traced(cmd, &secrets));
            }
            ...
```

Apply the same `let (mode, runner, trace, secrets) = snapshot(&ctx);` + `traced(cmd, &secrets)`
change to `ssh_probe`, `local_exec`, and `ssh_exec_all` (for `ssh_exec_all`, redact in the
`if trace` block if present; if it currently has `_trace`, keep ignoring but still destructure
`_secrets`).

- [ ] **Step 2: Add a trace-redaction test** to `exec.rs` tests:

```rust
    #[test]
    fn trace_redacts_registered_secret() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().register_secret("supersecretvalue");
        // We can't easily capture eprintln, so assert the redact path directly:
        let secrets = ctx.lock().unwrap().secrets.clone();
        let red = super::traced("docker login -p supersecretvalue", &secrets);
        assert_eq!(red, "docker login -p ***");
    }
```

- [ ] **Step 3: Run** `cargo test --bin nrg engine::builtins::exec` → PASS. Then `cargo test`.

- [ ] **Step 4: Engine clippy** `cargo clippy --all-targets 2>&1 | grep -E "src/engine|src/cli"` → empty.

- [ ] **Step 5: Commit**

```bash
git add src/engine/builtins/exec.rs
git commit -m "feat(secret): redact registered secret values in NRG_TRACE output"
```

---

## Task 5: Integration + acceptance + review

- [ ] **Step 1: Integration test** — Create `tests/secrets.rs`:

```rust
use assert_cmd::Command;
use std::fs;

#[test]
fn secret_is_usable_but_never_printed_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        let pw = secret("REGISTRY_PASSWORD");
        print("shown:" + pw.to_string());           // ***
        let r = local_exec("echo logged-in-with " + sh_quote(pw));
        print(r.stdout);                             // echoes the real value (we control this)
        "#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_realtokenvalue")
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicates::str::contains("shown:***"))
        .stderr(predicates::str::contains("logged-in-with ghp_realtokenvalue")); // via explicit sh_quote
}

#[test]
fn state_set_rejects_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("leak", secret("TOK"));"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_TOK", "sometokenvalue")
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure(); // Secret is not a String → state_set type error
}
```

- [ ] **Step 2: Run** `cargo test --test secrets` → PASS (2 tests). Then full `cargo test`.

- [ ] **Step 3: Commit**

```bash
git add tests/secrets.rs
git commit -m "test(secret): integration — Secret usable via sh_quote, never leaks, not persistable"
```

- [ ] **Step 4: Adversarial review workflow** — lenses: redaction completeness/bypass, `Secret` type leak paths (any registered op that reveals plaintext? interpolation `${}`? array/map embedding?), `sh_quote` correctness, lookup precedence/file parsing, forward-compat with the P5 stdlib (does the docker/registry stdlib have everything it needs to pass secrets safely — `--env-file`/`sh_quote`?). Fold fix-now, defer the rest.

---

## Self-review (author)

- **Spec §9 coverage:** tagged `Secret` redacted at use → T1/T3; reject too-short → T3; forbid
  `state_set(Secret)` → naturally (type mismatch), tested T5; keep secrets out of state → same;
  POSIX `sh_quote` newline/quote-safe → T1; trace redaction → T4. **Obviated by new arch:**
  remote-env-var elimination + `DEBUG`-trap gating (legacy `build_script`, deleted P6) — noted.
  **Deferred:** encoding-aware redaction (base64/url) and keeping secrets out of `http` URLs
  (P3 makes `http` ctx-aware; revisit redaction there).
- **Placeholders:** none. **Types:** `Secret`, `posix_quote`, `lookup_secret`, `redact`,
  `MIN_SECRET_LEN`, `RunCtx.secrets`, `register_secret`, `snapshot`(4-tuple), `traced` — used
  consistently T1–T5.
