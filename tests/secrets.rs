//! Integration: a Secret reaches the command plaintext (via sh_quote) but is redacted in all
//! of nrg's own output, and can't be persisted to state.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

#[test]
fn secret_usable_for_commands_but_redacted_in_output() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let outfile = dir.path().join("captured.txt");
    let script = format!(
        r#"
        let pw = secret("REGISTRY_PASSWORD");
        print("display:" + pw.to_string());                 // display:***  (Display)
        // sh_quote delivers the REAL value to the shell — capture it in a file we read back:
        local_exec("printf %s " + sh_quote(pw) + " > {out}");
        // ...but echoing the secret back into nrg's own output is redacted by on_print:
        let echoed = local_exec("printf 'echoed:%s' " + sh_quote(pw));
        print(echoed.stdout);                               // echoed:***
    "#,
        out = outfile.display()
    );
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_realtokenvalue")
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicate::str::contains("display:***"))
        .stderr(predicate::str::contains("echoed:***"))
        .stderr(predicate::str::contains("ghp_realtokenvalue").not()); // never in our output

    // The command itself received the real plaintext (proves sh_quote revealed it to the shell).
    let captured = fs::read_to_string(&outfile).unwrap();
    assert_eq!(captured, "ghp_realtokenvalue");
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
