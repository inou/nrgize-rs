//! Integration: the `nrg secrets` age pipeline round-trips (issue #27). This is the component
//! whose regression would destroy access to prod credentials, yet it had no end-to-end coverage
//! (the existing secrets test covers the engine `secret()` redaction type, not the subcommand).
//!
//! Gated on `age` + `age-keygen` being on PATH; skipped (passes) otherwise so CI without age
//! doesn't fail. Covers: key generation (parsing age-keygen's stderr for the pubkey), value
//! encrypt -> ENC[...] framing -> decrypt, and the file seal -> unseal path.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;
use std::path::Path;

fn age_available() -> bool {
    fn ok(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    ok("age") && ok("age-keygen")
}

#[test]
fn age_must_be_on_path_in_ci_or_this_files_coverage_silently_vanishes() {
    // Robustness review: "Age tests report pass when age is absent". Every other test in this
    // file gracefully self-skips (returns early, reporting PASS — not skip, not fail) when
    // `age`/`age-keygen` aren't on PATH, so a contributor's local machine without them installed
    // doesn't get spurious failures. But that same graceful skip means if the CI step that
    // installs `age` (.github/workflows/ci.yml, "Install age") were ever REMOVED (a step that
    // outright FAILS, e.g. an `apt-get` error, already turns CI red on its own — this canary's
    // unique value is the "step silently no longer runs, or installs somewhere off this job's
    // PATH" case, which wouldn't otherwise fail anything), this entire file's real end-to-end
    // coverage of the credential pipeline — key generation, the encrypt/decrypt round trip,
    // seal/unseal, the `secret()` ENC[...] resolution path — would silently disappear while the
    // build stayed all-green. This canary makes that specific regression loud instead of silent:
    // in CI (detected via the `CI` env var, set by GitHub Actions and effectively every other CI
    // provider), age's absence is a hard test failure; outside CI it stays a graceful skip, so
    // local dev without `age` installed still only sees one informational line, not a wall of
    // failures.
    if std::env::var("CI").is_err() {
        eprintln!(
            "skipping (not running in CI): only enforced as a hard failure when $CI is set, so \
             local dev without age/age-keygen installed doesn't get a failing test for it"
        );
        return;
    }
    assert!(
        age_available(),
        "`age`/`age-keygen` must be on PATH in CI — every other test in this file silently \
         reports PASS (not fail, not even skip) when they're absent, so if the CI step that \
         installs them (.github/workflows/ci.yml, \"Install age\") ever stopped running or \
         installed somewhere off this job's PATH, this file's entire coverage of the credential \
         pipeline would vanish with an all-green build. Check that CI step."
    );
}

fn nrg(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("nrg").unwrap();
    c.current_dir(dir);
    c
}

#[test]
fn secrets_value_encrypt_decrypt_round_trip() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    // 1. init generates the key pair (and parses the pubkey out of age-keygen's stderr).
    nrg(dir.path()).arg("secrets").arg("init").assert().success();
    assert!(dir.path().join(".nrg-key").exists(), ".nrg-key must exist after init");
    assert!(dir.path().join(".nrg-key.pub").exists(), ".nrg-key.pub must exist after init");

    // The private identity must be owner-only (issue #14).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join(".nrg-key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, ".nrg-key must be 0600");
    }

    // 2. encrypt a value -> ENC[...] token.
    let secret = "super-secret-prod-token-value";
    let out = nrg(dir.path())
        .arg("secrets")
        .arg("encrypt")
        .arg(secret)
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(token.starts_with("ENC[") && token.ends_with(']'), "bad token framing: {token}");

    // 3. decrypt the token -> original plaintext.
    let out = nrg(dir.path())
        .arg("secrets")
        .arg("decrypt")
        .arg(&token)
        .assert()
        .success()
        .get_output()
        .clone();
    let decrypted = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(decrypted, secret, "decrypt must recover the original plaintext");
}

#[test]
fn secret_transparently_decrypts_an_enc_token_pasted_into_env() {
    // Regression for the documented-but-previously-broken workflow (robustness review R3):
    // `nrg secrets encrypt` tells the user to paste the ENC[...] token into config/.env, but
    // secret() used to return that raw ciphertext verbatim instead of decrypting it.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success(); // .nrg-key also marks the project root

    let plaintext = "super-secret-prod-password-value";
    let out = nrg(dir.path())
        .arg("secrets")
        .arg("encrypt")
        .arg(plaintext)
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();

    fs::write(dir.path().join(".env"), format!("DB_PASSWORD={token}\n")).unwrap();

    let captured = dir.path().join("captured.txt");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"let pw = secret("DB_PASSWORD"); local_exec("printf %s " + sh_quote(pw) + " > {out}");"#,
            out = captured.display()
        ),
    )
    .unwrap();

    nrg(dir.path()).arg("exec").assert().success();

    let resolved = fs::read_to_string(&captured).unwrap();
    assert_eq!(
        resolved, plaintext,
        "secret() must resolve the DECRYPTED plaintext, not the raw ENC[...] ciphertext"
    );
}

#[test]
fn secret_reports_a_clear_error_when_enc_token_has_no_key_to_decrypt_it() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    // Isolate $HOME / $XDG_CONFIG_HOME to an empty directory: find_key_file() falls back to
    // ~/.config/nrg/key (or the platform equivalent) after its upward search, so on a machine
    // that has a REAL global nrg key configured, that fallback would find it and decryption
    // would fail for a DIFFERENT reason ("no identity matched") than the one this test asserts
    // on — this environment happens to have none, which would otherwise mask the gap.
    let fake_home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A DIFFERENT project generates the key/token, so this project's .env has an ENC[...]
    // token but no .nrg-key of its own to decrypt it with.
    let keydir = tempfile::tempdir().unwrap();
    let isolated_home = |c: &mut Command| {
        c.env("HOME", fake_home.path());
        c.env("XDG_CONFIG_HOME", fake_home.path().join("config"));
    };

    let mut init_cmd = nrg(keydir.path());
    isolated_home(&mut init_cmd);
    init_cmd.arg("secrets").arg("init").assert().success();

    let mut encrypt_cmd = nrg(keydir.path());
    isolated_home(&mut encrypt_cmd);
    let out = encrypt_cmd
        .arg("secrets")
        .arg("encrypt")
        .arg("whatever-value")
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();

    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".env"), format!("DB_PASSWORD={token}\n")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), r#"let pw = secret("DB_PASSWORD");"#).unwrap();

    let mut exec_cmd = nrg(dir.path());
    isolated_home(&mut exec_cmd);
    exec_cmd
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no .nrg-key was found"));
}

#[test]
fn decrypt_with_the_wrong_keys_identity_reports_ages_own_error_not_a_panic() {
    // Robustness review: "Secrets error paths ... untested" — a token encrypted for one
    // recipient, decrypted with a DIFFERENT project's private key (as opposed to no key at all,
    // already covered above), must surface age's own clear error rather than panicking or
    // silently returning garbage.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir_a = tempfile::tempdir().unwrap();
    nrg(dir_a.path()).arg("secrets").arg("init").assert().success();
    let out = nrg(dir_a.path())
        .arg("secrets")
        .arg("encrypt")
        .arg("only-decryptable-by-key-a")
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // A different project, with its OWN (non-matching) key pair.
    let dir_b = tempfile::tempdir().unwrap();
    nrg(dir_b.path()).arg("secrets").arg("init").assert().success();

    nrg(dir_b.path())
        .arg("secrets")
        .arg("decrypt")
        .arg(&token)
        .assert()
        .failure()
        .stderr(predicates::str::contains("no identity matched any of the recipients"));
}

#[test]
fn decrypt_rejects_malformed_armor_inside_a_well_framed_enc_token() {
    // A token with valid ENC[...] framing but garbage where the PEM-style armor should be —
    // e.g. hand-edited or corrupted in transit — must fail with age's own parse error, not
    // panic or silently return the garbage as if it were the plaintext.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success();

    nrg(dir.path())
        .arg("secrets")
        .arg("decrypt")
        .arg("ENC[this-is-not-real-age-armor-at-all]")
        .assert()
        .failure()
        .stderr(predicates::str::contains("age decrypt failed"))
        .stderr(predicates::str::contains("failed to read header"));
}

#[test]
fn init_warns_loudly_when_in_a_git_repo_without_gitignore_coverage() {
    // Robustness review: the .gitignore warning logic (src/cli/secrets.rs) had zero end-to-end
    // coverage — only the underlying pure functions were unit-tested (added alongside this
    // test). This proves the actual printed message a user sees.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap(); // looks like a real git work tree
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("is NOT in .gitignore"))
        .stdout(predicates::str::contains(".nrg-key"));
}

#[test]
fn init_gives_only_a_generic_reminder_when_gitignore_already_covers_the_key() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".gitignore"), ".nrg-key\n").unwrap();
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("Make sure").and(predicates::str::contains(".nrg-key")))
        .stdout(predicates::str::contains("is NOT in .gitignore").not());
}

#[test]
fn init_gives_only_a_generic_reminder_outside_any_git_repo() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap(); // no .git anywhere in this tree
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("Make sure"))
        .stdout(predicates::str::contains("is NOT in .gitignore").not());
}

#[test]
fn secrets_seal_unseal_round_trip() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success();

    // Seal an .env file, then unseal it and confirm the contents survive.
    let env_body = "DATABASE_URL=postgres://u:p@db/x\nAPI_KEY=abc123def456\n";
    fs::write(dir.path().join(".env"), env_body).unwrap();
    nrg(dir.path()).arg("secrets").arg("seal").arg(".env").assert().success();
    assert!(dir.path().join(".env.enc").exists(), ".env.enc must be produced");

    // Remove the plaintext, then unseal it back.
    fs::remove_file(dir.path().join(".env")).unwrap();
    nrg(dir.path()).arg("secrets").arg("unseal").arg(".env.enc").assert().success();
    let restored = fs::read_to_string(dir.path().join(".env")).unwrap();
    assert_eq!(restored, env_body, "unseal must recover the original .env contents");

    // Robustness review: the decrypted output must be owner-only (0600) at rest, matching the
    // private identity's own floor — `age -o` writes under the process umask otherwise.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join(".env")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "unsealed .env must be 0600");
    }
}

#[test]
fn unseal_refuses_to_clobber_an_existing_output_file_without_force() {
    // Robustness review: unseal used to silently overwrite an existing (possibly locally-edited)
    // .env the moment someone re-ran it, with no warning and no way to recover the prior contents.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success();

    fs::write(dir.path().join(".env"), "ORIGINAL=1\n").unwrap();
    nrg(dir.path()).arg("secrets").arg("seal").arg(".env").assert().success();

    // Locally edit .env AFTER sealing (simulating an operator's in-progress edit).
    fs::write(dir.path().join(".env"), "LOCALLY_EDITED=1\n").unwrap();

    nrg(dir.path())
        .arg("secrets")
        .arg("unseal")
        .arg(".env.enc")
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"))
        .stderr(predicates::str::contains("--force"));

    // The locally-edited content must survive the refused unseal.
    let contents = fs::read_to_string(dir.path().join(".env")).unwrap();
    assert_eq!(contents, "LOCALLY_EDITED=1\n", "a refused unseal must not touch the existing file");

    // --force explicitly opts into the overwrite.
    nrg(dir.path())
        .arg("secrets")
        .arg("unseal")
        .arg(".env.enc")
        .arg("--force")
        .assert()
        .success();
    let contents = fs::read_to_string(dir.path().join(".env")).unwrap();
    assert_eq!(contents, "ORIGINAL=1\n", "--force must overwrite with the sealed contents");
}

#[test]
fn encrypt_and_decrypt_read_from_stdin_when_the_value_is_omitted() {
    // Robustness review R8b: a secret value passed directly on argv is visible in `ps` and shell
    // history. Omitting the positional argument must read it from stdin instead.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success();

    let secret = "stdin-piped-secret-value";
    let out = nrg(dir.path())
        .arg("secrets")
        .arg("encrypt")
        .write_stdin(secret)
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(token.starts_with("ENC[") && token.ends_with(']'), "bad token framing: {token}");

    // Decrypt, also via stdin, and confirm the round trip — including that a trailing newline
    // (the shape a real pipe/heredoc produces) is stripped, not embedded into the ciphertext.
    let out = nrg(dir.path())
        .arg("secrets")
        .arg("decrypt")
        .write_stdin(format!("{token}\n"))
        .assert()
        .success()
        .get_output()
        .clone();
    let decrypted = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(decrypted, secret, "stdin-sourced encrypt/decrypt must round-trip correctly");
}

#[test]
fn encrypt_refuses_empty_input_from_both_argv_and_stdin() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path()).arg("secrets").arg("init").assert().success();

    nrg(dir.path())
        .arg("secrets")
        .arg("encrypt")
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicates::str::contains("No value to encrypt given"));
}
