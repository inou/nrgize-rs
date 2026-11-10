//! Integration: the `nrg secrets` age pipeline round-trips (issue #27). This is the component
//! whose regression would destroy access to prod credentials, yet it had no end-to-end coverage
//! (the existing secrets test covers the engine `secret()` redaction type, not the subcommand).
//!
//! Gated on `age` + `age-keygen` being on PATH; skipped (passes) otherwise so CI without age
//! doesn't fail. Covers: key generation (parsing age-keygen's stderr for the pubkey), value
//! encrypt -> ENC[...] framing -> decrypt, and `encrypt KEY` writing `.energize/secrets[.dest]`
//! that `secret()` then resolves.

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
    // Every other test in this
    // file gracefully self-skips (returns early, reporting PASS — not skip, not fail) when
    // `age`/`age-keygen` aren't on PATH, so a contributor's local machine without them installed
    // doesn't get spurious failures. But that same graceful skip means if the CI step that
    // installs `age` (.github/workflows/ci.yml, "Install age") were ever REMOVED (a step that
    // outright FAILS, e.g. an `apt-get` error, already turns CI red on its own — this canary's
    // unique value is the "step silently no longer runs, or installs somewhere off this job's
    // PATH" case, which wouldn't otherwise fail anything), this entire file's real end-to-end
    // coverage of the credential pipeline — key generation, the encrypt/decrypt round trip,
    // the `secret()` ENC[...] resolution path — would silently disappear while the
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
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();
    assert!(
        dir.path().join(".nrg-key").exists(),
        ".nrg-key must exist after init"
    );
    assert!(
        dir.path().join(".nrg-key.pub").exists(),
        ".nrg-key.pub must exist after init"
    );

    // The private identity must be owner-only (issue #14).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join(".nrg-key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, ".nrg-key must be 0600");
    }

    // 2. encrypt a value -> ENC[...] token.
    let secret = "super-secret-prod-token-value";
    let out = nrg(dir.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin(secret)
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        token.starts_with("ENC[") && token.ends_with(']'),
        "bad token framing: {token}"
    );

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
    assert_eq!(
        decrypted, secret,
        "decrypt must recover the original plaintext"
    );
}

#[cfg(unix)] // the ownership / mode check this asserts on is unix-only
#[test]
fn encrypt_refuses_a_world_writable_public_key_instead_of_encrypting_to_it() {
    // Key discovery used to accept the first `.nrg-key.pub` it found, with no check on who owns
    // it or who else can write it — so a file planted (or rewritten) by another local user became
    // the recipient of every secret, and the victim never saw a difference. Needs no `age` on
    // PATH: the refusal happens while resolving the key, before `age` is ever consulted.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let pubkey = dir.path().join(".nrg-key.pub");
    fs::write(
        &pubkey,
        "age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqqqqqqq\n",
    )
    .unwrap();
    fs::set_permissions(&pubkey, fs::Permissions::from_mode(0o666)).unwrap();

    let out = nrg(dir.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin("some-value-to-encrypt")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to use the key file"))
        .stderr(predicates::str::contains("writable by other users"))
        .stderr(predicates::str::contains(".nrg-key.pub"))
        .get_output()
        .clone();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("ENC["),
        "a refused key must produce no ciphertext at all"
    );
}

#[test]
fn encrypt_names_the_public_key_it_used_on_stderr_without_polluting_the_token_on_stdout() {
    // A substituted recipient is only detectable if the resolved public key is visible. It must
    // go to stderr: stdout carries the ENC[...] token the user copies or pipes.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

    let out = nrg(dir.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin("value-to-encrypt-here")
        .assert()
        .success()
        .stderr(predicates::str::contains(".nrg-key.pub"))
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().starts_with("ENC[") && stdout.trim().ends_with(']'),
        "stdout must be the bare token and nothing else: {stdout}"
    );
}

#[test]
fn init_warns_when_git_ignores_the_secrets_file() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !git(&["init", "-q"]) {
        eprintln!("skipping: git not available");
        return;
    }
    fs::write(dir.path().join(".gitignore"), ".energize/\n.nrg-key\n").unwrap();
    nrg(dir.path())
        .args(["secrets", "init"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            ".energize/secrets is git-ignored",
        ));

    let clean = tempfile::tempdir().unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(clean.path())
        .args(["init", "-q"])
        .output()
        .unwrap();
    fs::write(clean.path().join(".gitignore"), ".nrg-key\n").unwrap();
    nrg(clean.path())
        .args(["secrets", "init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("git-ignored").not());
}

fn init_project(dir: &Path) {
    nrg(dir).args(["secrets", "init"]).assert().success(); // .nrg-key also marks the root
}

fn encrypt_key(dir: &Path, args: &[&str], value: &str) {
    nrg(dir)
        .args(["secrets", "encrypt"])
        .args(args)
        .write_stdin(value)
        .assert()
        .success();
}

/// A script that writes `secret(name)`'s plaintext to `captured.txt` (nrg redacts it in output).
fn write_capture_script(dir: &Path, name: &str) -> std::path::PathBuf {
    let captured = dir.join("captured.txt");
    fs::write(
        dir.join("Energize.rhai"),
        format!(
            r#"local_exec("printf %s " + sh_quote(secret("{name}")) + " > {out}");"#,
            out = captured.display()
        ),
    )
    .unwrap();
    captured
}

#[test]
fn encrypt_key_writes_the_file_and_secret_resolves_it() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());

    let plaintext = "super-secret-prod-password-value";
    nrg(dir.path())
        .args(["secrets", "encrypt", "DB_PASSWORD"])
        .write_stdin(format!("{plaintext}\n"))
        .assert()
        .success()
        .stdout("")
        .stderr(predicates::str::contains(
            "Set DB_PASSWORD in .energize/secrets (ciphertext",
        ));

    let file = fs::read_to_string(dir.path().join(".energize/secrets")).unwrap();
    assert!(file.starts_with("DB_PASSWORD=ENC["), "{file}");
    assert!(!file.contains(plaintext));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join(".energize/secrets"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
    }

    let captured = write_capture_script(dir.path(), "DB_PASSWORD");
    nrg(dir.path()).arg("exec").assert().success();
    assert_eq!(
        fs::read_to_string(&captured).unwrap(),
        plaintext,
        "secret() must resolve the DECRYPTED plaintext, not the raw ENC[...] ciphertext"
    );
}

#[test]
fn encrypt_replaces_an_existing_key_and_keeps_the_rest_of_the_file() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    encrypt_key(dir.path(), &["A_KEY"], "first-value-aaa");
    encrypt_key(dir.path(), &["B_KEY"], "second-value-bbb");
    let path = dir.path().join(".energize/secrets");
    let with_comment = format!("# team secrets\n{}", fs::read_to_string(&path).unwrap());
    fs::write(&path, with_comment).unwrap();
    encrypt_key(dir.path(), &["A_KEY"], "replaced-value-aaa");

    let file = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines.len(), 3, "{file}");
    assert_eq!(lines[0], "# team secrets");
    assert!(lines[1].starts_with("A_KEY=ENC["), "{file}");
    assert!(lines[2].starts_with("B_KEY=ENC["), "{file}");

    let captured = write_capture_script(dir.path(), "A_KEY");
    nrg(dir.path()).arg("exec").assert().success();
    assert_eq!(fs::read_to_string(captured).unwrap(), "replaced-value-aaa");
}

#[test]
fn secret_file_resolves_from_a_subdirectory() {
    // The root-anchored file case: `.energize/secrets` is found from a nested working directory.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    encrypt_key(dir.path(), &["SUB_KEY"], "sub-dir-secret-value");
    let sub = dir.path().join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    let captured = write_capture_script(dir.path(), "SUB_KEY");
    fs::copy(dir.path().join("Energize.rhai"), sub.join("Energize.rhai")).unwrap();

    nrg(&sub).arg("exec").assert().success();
    assert_eq!(
        fs::read_to_string(captured).unwrap(),
        "sub-dir-secret-value"
    );

    // And `encrypt` run from the subdirectory writes the root's file, not a new one.
    encrypt_key(&sub, &["OTHER_KEY"], "other-secret-value");
    assert!(!sub.join(".energize").exists());
    assert!(fs::read_to_string(dir.path().join(".energize/secrets"))
        .unwrap()
        .contains("OTHER_KEY=ENC["));
}

#[test]
fn encrypt_dest_writes_the_dest_file_which_wins_over_the_shared_one() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    encrypt_key(dir.path(), &["DB_URL"], "shared-value-xyz");
    encrypt_key(
        dir.path(),
        &["DB_URL", "--dest", "staging"],
        "staging-value-xyz",
    );
    assert!(dir.path().join(".energize/secrets.staging").exists());
    let captured = write_capture_script(dir.path(), "DB_URL");

    nrg(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&captured).unwrap(), "staging-value-xyz");

    // No --dest: falls back to the shared file.
    nrg(dir.path()).arg("exec").assert().success();
    assert_eq!(fs::read_to_string(&captured).unwrap(), "shared-value-xyz");
}

#[test]
fn dest_run_falls_back_to_the_shared_file_for_a_key_its_own_file_lacks() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    encrypt_key(dir.path(), &["SHARED_KEY"], "shared-only-value");
    encrypt_key(
        dir.path(),
        &["DB_URL", "--dest", "staging"],
        "staging-value-xyz",
    );
    let captured = write_capture_script(dir.path(), "SHARED_KEY");

    nrg(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&captured).unwrap(), "shared-only-value");
}

#[test]
fn env_enc_token_is_refused() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    let out = nrg(dir.path())
        .args(["secrets", "encrypt", "--print", "K"])
        .write_stdin("whatever-value")
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    fs::write(dir.path().join("Energize.rhai"), r#"secret("TOK");"#).unwrap();

    nrg(dir.path())
        .env("NRG_SECRET_TOK", &token)
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "$NRG_SECRET_TOK holds an ENC[...] token",
        ));
}

#[test]
fn encrypt_validates_the_name_and_dest_before_doing_anything() {
    // No age needed: both refusals happen before a key is looked for.
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .args(["secrets", "encrypt", "bad-name"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid secret name"))
        .stderr(predicates::str::contains("bad-name").not());
    nrg(dir.path())
        .args(["secrets", "encrypt", "OK_NAME", "--dest", "../etc"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid --dest"));
    assert!(!dir.path().join(".energize/secrets").exists());
}

#[test]
fn encrypt_no_longer_takes_the_value_on_argv() {
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .args(["secrets", "encrypt", "KEY", "value-on-argv"])
        .assert()
        .failure();
}

#[cfg(unix)]
#[test]
fn encrypt_refuses_to_modify_an_untrusted_secrets_file() {
    use std::os::unix::fs::PermissionsExt;
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());
    encrypt_key(dir.path(), &["A_KEY"], "first-value-aaa");
    let path = dir.path().join(".energize/secrets");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    nrg(dir.path())
        .args(["secrets", "encrypt", "B_KEY"])
        .write_stdin("second-value-bbb")
        .assert()
        .failure()
        .stderr(predicates::str::contains("writable by other users"));
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
    // A DIFFERENT project generates the key/token, so this project's .energize/secrets has an ENC[...]
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
        .args(["encrypt", "--print", "K"])
        .write_stdin("whatever-value")
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();

    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join(".energize/secrets"),
        format!("DB_PASSWORD={token}\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"let pw = secret("DB_PASSWORD");"#,
    )
    .unwrap();

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
    // A token encrypted for one
    // recipient, decrypted with a DIFFERENT project's private key (as opposed to no key at all,
    // already covered above), must surface age's own clear error rather than panicking or
    // silently returning garbage.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir_a = tempfile::tempdir().unwrap();
    nrg(dir_a.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();
    let out = nrg(dir_a.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin("only-decryptable-by-key-a")
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // A different project, with its OWN (non-matching) key pair.
    let dir_b = tempfile::tempdir().unwrap();
    nrg(dir_b.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

    nrg(dir_b.path())
        .arg("secrets")
        .arg("decrypt")
        .arg(&token)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no identity matched any of the recipients",
        ));
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
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

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
    // The .gitignore warning logic (src/cli/secrets.rs) had zero end-to-end
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
fn encrypt_reads_the_value_from_stdin_and_decrypt_accepts_stdin_too() {
    // A secret value passed directly on argv is visible in `ps` and shell
    // history. Omitting the positional argument must read it from stdin instead.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

    let secret = "stdin-piped-secret-value";
    let out = nrg(dir.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin(secret)
        .assert()
        .success()
        .get_output()
        .clone();
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        token.starts_with("ENC[") && token.ends_with(']'),
        "bad token framing: {token}"
    );

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
    assert_eq!(
        decrypted, secret,
        "stdin-sourced encrypt/decrypt must round-trip correctly"
    );
}

#[test]
fn encrypt_refuses_empty_stdin() {
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

    nrg(dir.path())
        .arg("secrets")
        .args(["encrypt", "--print", "K"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicates::str::contains("No value to encrypt given"));
}

#[test]
fn decrypt_rejects_a_value_that_isnt_enc_framed_at_all() {
    // decrypt_value's "Invalid encrypted token format" branch — a plain
    // string missing the ENC[...] wrapper entirely — had no coverage. Must fail with a clear,
    // specific error rather than silently treating the raw string as if it were ciphertext.
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    nrg(dir.path())
        .arg("secrets")
        .arg("init")
        .assert()
        .success();

    nrg(dir.path())
        .arg("secrets")
        .arg("decrypt")
        .arg("just-a-plain-string-not-enc-framed")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Invalid encrypted token format"));
}

#[cfg(unix)]
#[test]
fn init_under_a_permissive_umask_leaves_a_usable_public_key() {
    // Issue #14: .nrg-key.pub used to follow the umask (0666 under umask 000), after which nrg's
    // own trust check refused it on every later encrypt.
    use std::os::unix::fs::PermissionsExt;
    if !age_available() {
        eprintln!("skipping: age/age-keygen not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("nrg");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("umask 000; exec \"$1\" secrets init")
        .arg("sh")
        .arg(&bin)
        .current_dir(dir.path())
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    let mode = |f: &str| {
        fs::metadata(dir.path().join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(".nrg-key"), 0o600);
    assert_eq!(mode(".nrg-key.pub"), 0o644);
    nrg(dir.path())
        .args(["secrets", "encrypt", "--print", "K"])
        .write_stdin("some-secret-value")
        .assert()
        .success();
}
