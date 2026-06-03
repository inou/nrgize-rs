//! Integration: state corruption is fatal, and a normal run persists state atomically.

use assert_cmd::Command;
use std::fs;

#[test]
fn exec_aborts_on_corrupt_state() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/state.json"), "{ broken json").unwrap();
    fs::write(dir.path().join("Energize.rhai"), r#"state_set("k", "v");"#).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("CORRUPT"));
}

#[test]
fn exec_persists_state_atomically() {
    let dir = tempfile::tempdir().unwrap();
    // `.energize` marks the project root and makes this the state home.
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("deploy.version", "v123"); print(state_get("deploy.version"));"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicates::str::contains("v123"));

    // State landed on disk in the versioned schema; no stray temp file.
    let raw = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(raw.contains("\"version\""));
    assert!(raw.contains("deploy.version"));
    assert!(!dir.path().join(".energize/state.json.tmp").exists());
}
