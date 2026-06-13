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
        // sim_http_healthy is the NEW-container probe: stubbed healthy (200) in dry-run.
        let r = sim_http_healthy("http://127.0.0.1:1/health");
        if !r.ok {{ throw "health failed" }}
        // http_get is HONEST even in dry-run (issue #16): an unreachable URL is status 0, NOT a
        // synthetic 200, so a precondition gate on existing reality can't be fooled by a plan.
        let real = http_get("http://127.0.0.1:1/never");
        if real.ok {{ throw "http_get must NOT report a synthetic ok in dry-run" }}
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
        .stdout(predicates::str::contains("probed live"))
        .stdout(predicates::str::contains("0 executed."));

    // No state.json was written (dry-run uses the overlay):
    assert!(!dir.path().join(".energize/state.json").exists());
    // The local_exec side effect did NOT happen:
    assert!(!sentinel.exists());
}
