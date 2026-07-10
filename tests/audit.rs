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
