//! Integration: the `nrg secrets` age pipeline round-trips (issue #27). This is the component
//! whose regression would destroy access to prod credentials, yet it had no end-to-end coverage
//! (the existing secrets test covers the engine `secret()` redaction type, not the subcommand).
//!
//! Gated on `age` + `age-keygen` being on PATH; skipped (passes) otherwise so CI without age
//! doesn't fail. Covers: key generation (parsing age-keygen's stderr for the pubkey), value
//! encrypt -> ENC[...] framing -> decrypt, and the file seal -> unseal path.

use assert_cmd::Command;
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
}
