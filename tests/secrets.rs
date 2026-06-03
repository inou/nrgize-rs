//! Integration: a Secret is usable via sh_quote, never printed plaintext, not persistable.

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
        .stderr(predicates::str::contains("logged-in-with ghp_realtokenvalue"));
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
        .failure(); // Secret is not a String -> state_set type error
}
