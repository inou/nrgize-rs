//! Integration: every LIVE `nrg exec`/`nrg run` appends an entry to `.energize/audit.log`,
//! `nrg audit` prints it, and `--dry-run` writes nothing (matching the "dry-run touches no
//! disk state" contract the rest of the suite already holds `nrg exec --dry-run` to).

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;

fn project(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    dir
}

const SCRIPT: &str = r#"
fn hello() { print("hi"); }
fn boom() { throw "kaboom"; }
"#;

#[test]
fn successful_run_is_recorded_and_shown_by_audit() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "hello"])
        .assert()
        .success();

    assert!(dir.path().join(".energize/audit.log").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("run hello"))
        .stdout(predicates::str::contains("success"));
}

#[test]
fn failed_run_is_recorded_with_its_error() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("run boom"))
        .stdout(predicates::str::contains("failed"))
        .stdout(predicates::str::contains("kaboom"));
}

#[test]
fn dry_run_writes_no_audit_entry() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "hello", "--dry-run"])
        .assert()
        .success();

    assert!(
        !dir.path().join(".energize/audit.log").exists(),
        "dry-run must not write the audit log, same as it writes no state"
    );
}

#[test]
fn audit_on_fresh_project_reports_no_history() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("No audit history yet"));
}

#[test]
fn audit_filter_narrows_to_matching_target() {
    let dir = project(SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).args(["run", "hello"]).assert().success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["audit", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("run hello"))
        .stdout(predicates::str::contains("boom").not());
}

#[test]
fn audit_entries_are_most_recent_first() {
    let dir = project(SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).args(["run", "hello"]).assert().success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hello_pos = stdout.find("run hello").unwrap();
    let boom_pos = stdout.find("run boom").unwrap();
    assert!(boom_pos < hello_pos, "most recent (boom) must print first:\n{stdout}");
}

/// The audit log's headline safety property: a secret revealed into a thrown error must never
/// reach `.energize/audit.log` in plaintext, on disk or in `nrg audit`'s output. Mirrors the
/// same `ctx.secrets`-redaction boundary the dry-run plan already goes through.
#[test]
fn secret_revealed_into_a_thrown_error_is_redacted_from_the_audit_log() {
    let dir = project(
        r#"
fn boom() {
    let s = secret("DBPASS");
    throw "boom: " + reveal(s);
}
"#,
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_DBPASS", "hunter2supersecretvalue")
        .args(["run", "boom"])
        .assert()
        .failure();

    let raw = fs::read_to_string(dir.path().join(".energize/audit.log")).unwrap();
    assert!(
        !raw.contains("hunter2supersecretvalue"),
        "secret plaintext must never land in audit.log on disk:\n{raw}"
    );
    assert!(raw.contains("***"), "a redaction marker should stand in for the secret:\n{raw}");

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("hunter2supersecretvalue").not());
}

/// Same property from the OTHER direction: an operator-typed CLI arg that happens to equal a
/// value the script separately resolved via `secret()` must also be redacted from `entry.args`,
/// not just from the thrown-error path above.
#[test]
fn cli_arg_matching_a_registered_secret_is_redacted_from_audit_args() {
    let dir = project(
        r#"
fn rollback(pw) {
    let s = secret("DBPASS"); // registers the plaintext for redaction, regardless of `pw`
    print("rolling back");
}
"#,
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_DBPASS", "hunter2supersecretvalue")
        .args(["run", "rollback", "hunter2supersecretvalue"])
        .assert()
        .success();

    let raw = fs::read_to_string(dir.path().join(".energize/audit.log")).unwrap();
    assert!(
        !raw.contains("hunter2supersecretvalue"),
        "a CLI arg matching a registered secret must be redacted from audit.log:\n{raw}"
    );
}
