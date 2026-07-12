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
        print("debug:" + pw.to_debug());                    // debug:***  (redacted rendering)
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
        .stderr(predicate::str::contains("debug:***"))
        .stderr(predicate::str::contains("echoed:***"))
        .stderr(predicate::str::contains("ghp_realtokenvalue").not()); // never in our output

    // The command itself received the real plaintext (proves sh_quote revealed it to the shell).
    let captured = fs::read_to_string(&outfile).unwrap();
    assert_eq!(captured, "ghp_realtokenvalue");
}

#[test]
fn secret_cmd_framing_fetches_via_a_local_command() {
    // Roadmap 2.4 step 2: a CMD[...]-framed value in .energize/secrets is a fetch-adapter
    // command (Kamal-style — 1Password/Bitwarden/Vault/Doppler all reduce to "run some CLI,
    // capture its stdout"). No real vendor CLI is needed to prove the mechanism: `echo` stands
    // in for `op read ...`/`vault kv get ...`/etc, since the codepath treats the command as
    // opaque either way. Both halves of the sibling test above are proven here too: the fetched
    // plaintext (a) reaches the shell for real (captured to a file, since `sh_quote` delivers
    // the true value there) and (b) is actually registered for redaction — echoing it back
    // through nrg's own `print` must come out "echoed:***", not the plaintext; a
    // `to_debug()`-only check wouldn't catch a missing `ctx.secrets.insert(...)`, since Secret's
    // debug rendering is hardcoded regardless of registration (Opus review).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join(".energize/secrets"),
        "OP_TOKEN=CMD[echo -n fetched-secret-value-123]\n",
    )
    .unwrap();
    let outfile = dir.path().join("captured.txt");
    let script = format!(
        r#"
        let pw = secret("OP_TOKEN");
        print("debug:" + pw.to_debug());
        local_exec("printf %s " + sh_quote(pw) + " > {out}");
        let echoed = local_exec("printf 'echoed:%s' " + sh_quote(pw));
        print(echoed.stdout);
    "#,
        out = outfile.display()
    );
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicate::str::contains("debug:***"))
        .stderr(predicate::str::contains("echoed:***"))
        .stderr(predicate::str::contains("fetched-secret-value-123").not());

    let captured = fs::read_to_string(&outfile).unwrap();
    assert_eq!(captured, "fetched-secret-value-123");
}

#[test]
fn secret_cmd_framing_surfaces_a_fetch_command_failure() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join(".energize/secrets"),
        "OP_TOKEN=CMD[sh -c 'echo not signed in >&2; exit 1']\n",
    )
    .unwrap();
    fs::write(dir.path().join("Energize.rhai"), r#"secret("OP_TOKEN");"#).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not signed in"));
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
