//! Integration: `nrg exec --dry-run` records a plan and makes no changes.

use assert_cmd::Command;
use std::fs;

#[test]
fn dry_run_records_plan_and_makes_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let sentinel = dir.path().join("should-not-exist");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
        local_exec("echo build > {sentinel}");
        state_set("version", "v9");
        let r = http_get("http://127.0.0.1:1/health");  // unreachable, but dry-run => ok
        if !r.ok {{ throw "health failed" }}
        "#,
            sentinel = sentinel.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stdout(predicates::str::contains("PLAN (dry run"))
        .stdout(predicates::str::contains("version = v9"))
        .stdout(predicates::str::contains("0 executed."));

    // No state.json was written (dry-run uses the overlay):
    assert!(!dir.path().join(".energize/state.json").exists());
    // The local_exec side effect did NOT happen:
    assert!(!sentinel.exists());
}
