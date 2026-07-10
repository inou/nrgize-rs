//! Integration: `nrg status` reads state.json and reports service/host info, with `--offline`
//! skipping the live per-host probe (no network access needed for these tests).

use assert_cmd::Command;
use std::fs;

fn project_with_state(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    dir
}

const SEED_SCRIPT: &str = r#"
state_set("app.version", "v42");
state_set("app.image", "ghcr.io/org/app:v42");
state_set("app.deployed_at", "2026-07-10T00:00:00Z");
state_set("app.target.web1", "localhost:13000");
"#;

#[test]
fn status_reports_recorded_service_offline() {
    let dir = project_with_state(SEED_SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "app", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("app"))
        .stdout(predicates::str::contains("v42"))
        .stdout(predicates::str::contains("ghcr.io/org/app:v42"))
        .stdout(predicates::str::contains("web1"))
        .stdout(predicates::str::contains("[offline]"));
}

#[test]
fn status_with_no_service_arg_discovers_all_services() {
    let dir = project_with_state(&format!(
        "{SEED_SCRIPT}\nstate_set(\"worker.version\", \"v7\");"
    ));
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("app"))
        .stdout(predicates::str::contains("worker"))
        .stdout(predicates::str::contains("v7"));
}

#[test]
fn status_on_fresh_project_reports_nothing_deployed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No deployed services found"));
}

#[test]
fn status_on_unknown_service_shows_no_deploy_recorded() {
    let dir = project_with_state(SEED_SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "ghost", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("none — no deploy recorded"));
}
